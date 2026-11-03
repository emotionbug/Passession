import 'dart:async';

import 'package:flutter/foundation.dart';

import '../../core/client.dart';
import '../../core/json.dart';
import '../../core/target.dart';
import 'git_actions.dart' show CheckoutGitState;

String _responseTypeFor(String type) => type.endsWith('_request')
    ? type.replaceFirst(RegExp(r'_request$'), '_response')
    : type.replaceFirst(RegExp(r'\.request$'), '.response');

Future<JsonMap> gitRequest(
  PaseoClient client,
  String type,
  JsonMap fields,
) async {
  final response = await client.request(type, fields, responseType: _responseTypeFor(type));
  final error = response['error'];
  if (error != null) {
    throw StateError(
      error is Map
          ? stringValue(error['message'], error.toString())
          : error.toString(),
    );
  }
  if (response['success'] == false || response['accepted'] == false) {
    throw StateError('The host did not accept this operation.');
  }
  return response;
}

/// The response payload of a checkout RPC even when it carries a domain error
/// (`error`, `success: false`), which the original clients read from the
/// payload; the client raises those as [DaemonException] with the payload.
/// Transport and `rpc_error` failures still throw.
Future<JsonMap> checkoutPayload(PaseoClient client, String type, JsonMap fields) async {
  try {
    return await client.request(type, fields, responseType: _responseTypeFor(type));
  } on DaemonException catch (error) {
    final payload = error.payload;
    if (payload == null || payload.containsKey('requestType')) rethrow;
    return payload;
  }
}

/// A checkout payload's error message (`error.message` or a string error).
String? checkoutPayloadError(JsonMap payload) {
  final error = payload['error'];
  if (error == null) return null;
  return error is Map ? stringValue(error['message'], error.toString()) : error.toString();
}

/// Original `useWorkingDiffComparison` (git/working-diff-comparison): the
/// selected comparison is scoped to the workspace checkout, not a panel, so
/// every Changes view of one workspace shows the same comparison. Unselected
/// checkouts show "all". In memory only, like the original zustand store.
class WorkingDiffComparisons extends ChangeNotifier {
  WorkingDiffComparisons._();
  static final instance = WorkingDiffComparisons._();
  final Map<String, String> _overrides = {};

  static String key(String serverId, String workspaceId, String cwd) {
    final workspace = workspaceId.trim();
    final trimmed = cwd.trim();
    final normalized = trimmed == '/' ? trimmed : trimmed.replaceFirst(RegExp(r'/+$'), '');
    final checkout = workspace.isNotEmpty
        ? 'workspace=${Uri.encodeComponent(workspace)}'
        : 'cwd=${Uri.encodeComponent(normalized)}';
    return 'working-diff:server=${Uri.encodeComponent(serverId.trim())}:$checkout';
  }

  String resolve(String serverId, String workspaceId, String cwd) =>
      _overrides[key(serverId, workspaceId, cwd)] ?? 'all';

  void select(String serverId, String workspaceId, String cwd, String comparison) {
    final id = key(serverId, workspaceId, cwd);
    if (_overrides[id] == comparison) return;
    _overrides[id] = comparison;
    notifyListeners();
  }

  @visibleForTesting
  void reset() => _overrides.clear();
}

/// Original `normalizeCheckoutDiffCompare`: the uncommitted comparison never
/// carries a base ref; other comparisons carry the trimmed one when present.
JsonMap normalizeCheckoutDiffCompare({
  required String mode,
  String? baseRef,
  required bool ignoreWhitespace,
  String? detail,
}) {
  final normalized = <String, dynamic>{
    'mode': mode,
    'ignoreWhitespace': ignoreWhitespace,
    if (detail == 'summary') 'detail': 'summary',
  };
  if (mode == 'uncommitted') return normalized;
  final trimmed = baseRef?.trim() ?? '';
  return trimmed.isEmpty ? normalized : {...normalized, 'baseRef': trimmed};
}

class GitController extends ChangeNotifier {
  /// A working diff reads status through its checkout's shared [checkout]
  /// state, on the same client, so the view and the state send one request.
  GitController(PaseoClient client, this.target, {CheckoutGitState? checkout})
      : _client = client,
        _checkout = checkout {
    assert(checkout == null || identical(checkout.client, client));
    mode = isCommit
        ? 'all'
        : _comparisons.resolve(target.serverId, target.workspaceId, target.cwd);
    selectedPath = target.data.text(
      'path',
      target.kind == 'working_diff' ? target.id ?? '' : '',
    );
    _listen();
    if (!isCommit) _comparisons.addListener(_comparisonChanged);
    // Original runCheckoutAction: a finished action invalidates the checkout's
    // status and commits (diffs stay subscription-fed).
    _invalidations = CheckoutInvalidations.stream.listen((checkout) {
      if (checkout.$1 == target.serverId && checkout.$2 == target.cwd) {
        if (_active) {
          unawaited(_reloadStatus());
        } else {
          _missed();
        }
      }
    });
  }

  void _listen() {
    _connectionStatus = client.status;
    client.addListener(_connectionChanged);
    _events = client.events.listen((event) {
      final p = jsonMap(event['payload']);
      if (event['type'] == 'checkout_status_update' &&
          p.text('cwd') == target.cwd) {
        _statusRevision++;
        final previousBase = status?.text('baseRef');
        status = p;
        notifyListeners();
        // Original applyCheckoutStatusUpdateFromEvent invalidates the commit list.
        if (_commitsLoaded) {
          if (_active) {
            unawaited(loadCommits().catchError((Object _) {}));
          } else {
            _missed();
          }
        }
        // The diff is keyed by the status base ref.
        if (!isCommit && previousBase != null && previousBase != p.text('baseRef')) {
          baseRef = p.text('baseRef');
          unawaited(refresh());
        }
      }
    });
  }

  /// Moves this view to the host's replacement client (a connection switch)
  /// and that client's shared [checkout] state, keeping the cached repository
  /// state until the new client reloads it.
  void rebind(PaseoClient next, {CheckoutGitState? checkout}) {
    assert(checkout == null || identical(checkout.client, next));
    if (_disposed || identical(next, _client)) return;
    client.removeListener(_connectionChanged);
    unawaited(_events?.cancel());
    unawaited(_observation?.close());
    _observation = null;
    _client = next;
    _checkout = checkout;
    _generation++;
    _connectionRevision++;
    _abandonLoads();
    _listen();
    if (_active) {
      unawaited(refresh(resume: true));
      // The resumed diff does not reload a working diff's commit list.
      if (_commitsLoaded && !isCommit) {
        unawaited(loadCommits().catchError((Object _) {}));
      }
    } else {
      _missed();
    }
  }

  /// Records a checkout change this hidden view missed: it reloads when shown,
  /// and so does a commit list it had loaded.
  void _missed() {
    _stale = true;
    if (_commitsLoaded) _commitsStale = true;
  }

  /// Original `useCheckoutDiffQuery` gates its diff subscription on
  /// `useRetainedPanelActive`: a hidden retained Changes view keeps its cached
  /// status, files and patches, but holds no diff subscription and issues no
  /// checkout RPCs; results still in flight when it hides are dropped. What
  /// it missed meanwhile reloads once when it is shown again.
  bool get active => _active;
  set active(bool value) {
    if (_disposed || value == _active) return;
    _active = value;
    if (!value) {
      // Late results of the hidden view's loads would land on a comparison or
      // checkout state it no longer tracks.
      _stale = _stale || loading || !isCommit;
      // Placeholders that requested a patch request once; an interrupted load
      // has them remount when the view is shown.
      if (patchLoading || loadingPatches.isNotEmpty) _patchLoadsDeferred = true;
      _generation++;
      unawaited(_observation?.close());
      _observation = null;
      _abandonLoads();
      notifyListeners();
      return;
    }
    final deferred = _patchLoadsDeferred;
    final commits = _commitsStale;
    _patchLoadsDeferred = false;
    _commitsStale = false;
    if (_stale) {
      // The reload's new snapshot (or, for a commit, its file list) remounts
      // the placeholders whose loads were deferred while hidden.
      _stale = false;
      unawaited(refresh(resume: true));
    } else if (deferred) {
      _patchRevision++;
      notifyListeners();
    }
    // A commit view's reload includes its commit list.
    if (commits && !isCommit) {
      unawaited(loadCommits().catchError((Object _) {}));
    }
  }

  void _abandonLoads() {
    _fullRequest = null;
    loadingPatches.clear();
    loading = false;
    patchLoading = false;
  }

  bool _active = true;

  /// Something this view depends on changed, or a load was interrupted, while
  /// it was hidden.
  bool _stale = false;
  bool _patchLoadsDeferred = false;

  /// A loaded commit list missed a change while this view was hidden.
  bool _commitsStale = false;
  WorkingDiffComparisons get _comparisons => WorkingDiffComparisons.instance;
  StreamSubscription<(String, String)>? _invalidations;
  bool _commitsLoaded = false;
  PaseoClient _client;
  PaseoClient get client => _client;
  CheckoutGitState? _checkout;

  /// The checkout status, joined with the shared state's read when bound to
  /// one. Its payload carries domain failures, which this view reports as
  /// errors like its own request does.
  Future<JsonMap> _readStatus() async {
    final checkout = _checkout;
    if (checkout == null) {
      return gitRequest(client, 'checkout_status_request', {'cwd': target.cwd});
    }
    final payload = await checkout.readStatus();
    final failure = checkoutPayloadError(payload);
    if (failure != null) throw StateError(failure);
    if (payload['success'] == false || payload['accepted'] == false) {
      throw StateError('The host did not accept this operation.');
    }
    return payload;
  }

  final PanelTarget target;
  StreamSubscription<JsonMap>? _events;
  OwnedObservation? _observation;
  late ConnectionStatus _connectionStatus;
  int _connectionRevision = 0;
  int _statusRevision = 0;
  JsonMap? status;
  List<JsonMap> files = [];
  List<JsonMap> commits = [];
  final Map<String, JsonMap> patches = {};
  /// Per-file lazy loads issued by the scrolling diff document.
  final Set<String> loadingPatches = {};
  final Map<String, Object> patchErrors = {};
  String mode = 'all';
  String baseRef = '';
  String selectedPath = '';
  bool ignoreWhitespace = false;
  bool loading = false;
  bool patchLoading = false;
  bool tooLarge = false;
  Object? error;
  /// The diff snapshot's own error (original `diffPayloadError`).
  String? diffError;
  Object? patchError;
  int _generation = 0;

  /// Bumped whenever cached patches and in-flight patch loads are invalidated;
  /// patch placeholders are keyed by it so they request again.
  int get patchRevision => _patchRevision;
  int _patchRevision = 0;
  int _selection = 0;
  Future<JsonMap>? _fullRequest;
  bool _disposed = false;
  bool get isCommit => target.kind == 'commit_diff';
  String get sha => target.data.text('sha', target.id ?? '');
  JsonMap get compare => normalizeCheckoutDiffCompare(
    mode: mode,
    baseRef: baseRef,
    ignoreWhitespace: ignoreWhitespace,
  );

  /// Selects [comparison] for every Changes view of this workspace.
  void selectComparison(String comparison) =>
      _comparisons.select(target.serverId, target.workspaceId, target.cwd, comparison);

  void _comparisonChanged() {
    final next = _comparisons.resolve(target.serverId, target.workspaceId, target.cwd);
    if (_disposed || next == mode) return;
    mode = next;
    unawaited(refresh());
  }

  void _connectionChanged() {
    if (_disposed || _connectionStatus == client.status) return;
    _connectionStatus = client.status;
    _connectionRevision++;
    if (_connectionStatus == ConnectionStatus.connected) {
      if (!_active) {
        _missed();
        return;
      }
      // Owned observations restore their own baseline; only the independent RPC
      // state needs reloading here. An initial offline open has no observation.
      unawaited(_observation == null ? refresh() : _reloadStatus());
    } else {
      _patchRevision++;
      _fullRequest = null;
      patches.clear();
      loading = false;
      patchLoading = false;
      error = StateError(
        'Host disconnected. Repository state is cached until reconnection.',
      );
      notifyListeners();
    }
  }

  Future<void> _reloadStatus() async {
    final connection = _connectionRevision;
    final revision = _statusRevision;
    error = null;
    loading = true;
    notifyListeners();
    try {
      final next = await _readStatus();
      if (_disposed || connection != _connectionRevision) return;
      if (revision == _statusRevision) status = next;
      notifyListeners();
      if (_commitsLoaded) {
        if (_active) {
          await loadCommits();
        } else {
          _missed();
        }
      }
    } catch (e) {
      if (!_disposed && connection == _connectionRevision) {
        error = e;
        notifyListeners();
      }
    } finally {
      if (!_disposed && connection == _connectionRevision) {
        loading = false;
        notifyListeners();
      }
    }
  }

  /// Reloads the status and resubscribes to the diff. A [resume] (shown again,
  /// or moved to a new client) keeps the cached patches until the new
  /// snapshot replaces them.
  Future<void> refresh({bool resume = false}) async {
    if (!_active) {
      _stale = true;
      return;
    }
    final generation = ++_generation;
    final statusRevision = _statusRevision;
    loading = true;
    error = null;
    patchError = null;
    if (!resume) {
      patches.clear();
      patchErrors.clear();
      _patchRevision++;
    }
    loadingPatches.clear();
    _fullRequest = null;
    patchLoading = false;
    notifyListeners();
    try {
      final next = await _readStatus();
      if (_disposed || generation != _generation) return;
      if (statusRevision == _statusRevision) status = next;
      final current = status!;
      if (!isCommit) baseRef = current.text('baseRef');
      notifyListeners();
      if (!current.flag('isGit')) return;
      if (isCommit) {
        await loadCommits();
        if (_disposed || generation != _generation) return;
        final commit = commits.where((c) => c.text('sha') == sha).firstOrNull;
        files = commit?.array('files').map(jsonMap).toList() ?? [];
        if (files.isEmpty && selectedPath.isNotEmpty) {
          files = [
            {'path': selectedPath},
          ];
        }
        // A commit's patches never change; placeholders left by interrupted
        // loads request again.
        if (resume) _patchRevision++;
      } else if (mode == 'all' && !client.supports('checkoutDiffAll')) {
        // COMPAT(checkoutDiffAll): hosts without All changes report an update prompt.
        await _observation?.close();
        _observation = null;
        _summary({
          'files': const [],
          'error': {'code': 'UNKNOWN', 'message': 'Update the host to view All changes'},
        });
      } else {
        await _observation?.close();
        _observation = null;
        final observation = await client.observe(
          type: 'subscribe_checkout_diff_request',
          fields: {
            'cwd': target.cwd,
            'compare': {...compare, 'detail': 'summary'},
          },
          responseType: 'subscribe_checkout_diff_response',
          onEvent: (payload) {
            if (_disposed || generation != _generation) return;
            if (payload['type'] == 'subscription.error') {
              error = jsonMap(payload['payload']).text('error');
              notifyListeners();
              return;
            }
            _summary(jsonMap(payload['payload']));
            if (payload['type'] == 'subscription.restored' &&
                selectedPath.isNotEmpty) {
              unawaited(selectFile(selectedPath));
            }
          },
        );
        if (_disposed || generation != _generation) {
          await observation.close();
          return;
        }
        _observation = observation;
        _summary(observation.snapshot);
      }
      if (selectedPath.isNotEmpty) unawaited(selectFile(selectedPath));
    } catch (e) {
      if (!_disposed && generation == _generation) error = e;
    } finally {
      if (!_disposed && generation == _generation) {
        loading = false;
        notifyListeners();
      }
    }
  }

  void _summary(JsonMap payload) {
    final payloadError = payload['error'];
    diffError = payloadError == null
        ? null
        : stringValue(jsonMap(payloadError)['message'], payloadError.toString());
    files = payload.array('files').map(jsonMap).toList();
    tooLarge = payload.flag('diffTooLarge');
    patches.clear();
    loadingPatches.clear();
    patchErrors.clear();
    _patchRevision++;
    _fullRequest = null;
    notifyListeners();
  }

  Future<void> loadCommits() async {
    _commitsLoaded = true;
    final connection = _connectionRevision;
    final result = await gitRequest(client, 'checkout.commits.list.request', {
      'cwd': target.cwd,
    });
    if (_disposed || connection != _connectionRevision) return;
    commits = result.array('commits').map(jsonMap).toList();
    notifyListeners();
  }

  Future<void> selectFile(String path) async {
    selectedPath = path;
    patchError = null;
    final selection = ++_selection;
    if (!_active) {
      _patchLoadsDeferred = true;
      return;
    }
    if (patches.containsKey(path)) {
      patchLoading = false;
      notifyListeners();
      return;
    }
    final generation = _generation;
    final revision = _patchRevision;
    patchLoading = true;
    notifyListeners();
    try {
      await _fetch(path, generation, revision);
    } catch (e) {
      if (!_disposed && generation == _generation && selection == _selection) {
        patchError = e;
      }
    } finally {
      if (!_disposed && generation == _generation && selection == _selection) {
        patchLoading = false;
        notifyListeners();
      }
    }
  }

  /// Loads one file's patch without changing the selection, for files scrolled
  /// into view. Working diffs share the single full-diff request.
  Future<void> ensurePatch(String path) async {
    if (patches.containsKey(path) || loadingPatches.contains(path)) return;
    if (!_active) {
      _patchLoadsDeferred = true;
      return;
    }
    final generation = _generation;
    final revision = _patchRevision;
    loadingPatches.add(path);
    patchErrors.remove(path);
    notifyListeners();
    try {
      await _fetch(path, generation, revision);
      if (!_disposed &&
          generation == _generation &&
          revision == _patchRevision &&
          !tooLarge &&
          !patches.containsKey(path)) {
        // The host omitted this file from the full diff: it changed after the summary.
        patchErrors[path] = const PatchUnavailable();
      }
    } catch (e) {
      if (!_disposed && generation == _generation && revision == _patchRevision) {
        patchErrors[path] = e;
      }
    } finally {
      if (!_disposed && generation == _generation && revision == _patchRevision) {
        loadingPatches.remove(path);
        notifyListeners();
      }
    }
  }

  Future<void> _fetch(String path, int generation, int revision) async {
    if (isCommit) {
      final result = await gitRequest(
        client,
        'checkout.commits.file_diff.request',
        {'cwd': target.cwd, 'sha': sha, 'path': path},
      );
      if (_disposed ||
          generation != _generation ||
          revision != _patchRevision) {
        return;
      }
      patches[path] = result['file'] == null
          ? {'path': path, 'hunks': [], 'status': 'binary'}
          : jsonMap(result['file']);
    } else {
      // Working patches use the existing full-diff RPC, deferred until a file opens.
      // Share one request across rapid file switches and reject stale summary revisions.
      final request = _fullRequest ??= checkoutPayload(
        client,
        'checkout.diff.get.request',
        {
          'cwd': target.cwd,
          'compare': {...compare, 'detail': 'full'},
        },
      );
      JsonMap result;
      try {
        result = await request;
      } finally {
        if (identical(_fullRequest, request)) _fullRequest = null;
      }
      if (_disposed ||
          generation != _generation ||
          revision != _patchRevision) {
        return;
      }
      // Original diffTooLarge: the host omits every file and the whole body shows
      // the too-large state rather than a per-file failure.
      if (result.flag('diffTooLarge')) {
        tooLarge = true;
        notifyListeners();
        return;
      }
      final failure = result['error'];
      if (failure != null) {
        throw StateError(
          failure is Map ? stringValue(failure['message'], failure.toString()) : failure.toString(),
        );
      }
      for (final file in result.array('files').map(jsonMap)) {
        patches[file.text('path')] = file;
      }
    }
  }

  @override
  void dispose() {
    _disposed = true;
    _generation++;
    client.removeListener(_connectionChanged);
    _comparisons.removeListener(_comparisonChanged);
    _events?.cancel();
    _invalidations?.cancel();
    _observation?.close();
    super.dispose();
  }
}

/// Checkouts whose git state a finished action changed (original
/// `invalidateCheckoutGitQueriesForClient`): `(serverId, cwd)` pairs.
abstract final class CheckoutInvalidations {
  static final _controller = StreamController<(String, String)>.broadcast(sync: true);
  static final Map<String, int> _revisions = {};
  static Stream<(String, String)> get stream => _controller.stream;

  /// Bumped before listeners run, so a status read can tell whether it began
  /// after the checkout's latest invalidation whichever listener asks first.
  static int revision(String serverId, String cwd) => _revisions['$serverId::$cwd'] ?? 0;

  static void add(String serverId, String cwd) {
    final key = '$serverId::$cwd';
    _revisions[key] = revision(serverId, cwd) + 1;
    _controller.add((serverId, cwd));
  }
}

class PatchUnavailable implements Exception {
  const PatchUnavailable();
  @override
  String toString() => 'This file changed since the summary. Refresh to load its current patch.';
}
