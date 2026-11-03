import 'dart:async';
import 'dart:convert';

import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:shared_preferences/shared_preferences.dart';

import 'package:paseo_sessions/core/app_store.dart';
import 'package:paseo_sessions/core/client.dart';
import 'package:paseo_sessions/core/host_config.dart';
import 'package:paseo_sessions/core/json.dart';
import 'package:paseo_sessions/core/scope.dart';
import 'package:paseo_sessions/core/target.dart';
import 'package:paseo_sessions/features/git/git_actions.dart';
import 'package:paseo_sessions/features/git/git_controller.dart';
import 'package:paseo_sessions/features/git/pull_request_panel.dart';
import 'package:paseo_sessions/ui/localization.dart';
import 'package:paseo_sessions/ui/panel_activity.dart';
import 'package:paseo_sessions/ui/theme.dart';

const _host = HostConfig(
  id: 'host',
  label: 'host',
  endpoint: 'ws://127.0.0.1:1/ws',
);

const _workingDiff = PanelTarget(
  kind: 'working_diff',
  serverId: 'host',
  workspaceId: 'main',
  cwd: '/repo',
);

const _pullRequest = PanelTarget(
  kind: 'pull_request',
  serverId: 'host',
  workspaceId: 'main',
  cwd: '/repo',
);

Future<PaseoClient> _connect(WidgetTester tester, _Wire wire) async {
  final client = PaseoClient(
    host: _host,
    clientId: 'git-pane-activity',
    autoReconnect: false,
    connector: (endpoint, {headers = const {}, protocols}) async => wire,
  );
  final connecting = client.connect();
  for (var i = 0; i < 5; i++) {
    await tester.pump(const Duration(milliseconds: 10));
  }
  await connecting;
  return client;
}

Future<void> _settle(WidgetTester tester) async {
  for (var i = 0; i < 10; i++) {
    await tester.pump(const Duration(milliseconds: 10));
  }
}

void main() {
  testWidgets(
    'a hidden Changes view releases its diff subscription, drops late results and reloads once shown',
    (tester) async {
      final fullDiffs = <Completer<JsonMap>>[];
      var snapshots = 0;
      final wire = _Wire(
        features: const {'ownedSubscriptions': true, 'checkoutDiffAll': true},
        respond: (request) {
          switch (request['type']) {
            case 'checkout_status_request':
              return {'cwd': '/repo', 'isGit': true, 'baseRef': 'main'};
            case 'subscribe_checkout_diff_request':
              snapshots++;
              return {
                'subscriptionId': 'diff-$snapshots',
                'cwd': '/repo',
                'files': [
                  {'path': snapshots == 1 ? 'a.txt' : 'b.txt', 'additions': 1},
                ],
              };
            case 'checkout.diff.get.request':
              final response = Completer<JsonMap>();
              fullDiffs.add(response);
              return response.future;
          }
          return {};
        },
      );
      final client = await _connect(tester, wire);
      final git = GitController(client, _workingDiff);
      try {
        unawaited(git.refresh());
        await _settle(tester);
        expect(wire.count('subscribe_checkout_diff_request'), 1);
        expect(git.files.single.text('path'), 'a.txt');
        unawaited(git.ensurePatch('a.txt'));
        await _settle(tester);
        expect(fullDiffs, hasLength(1));

        git.active = false;
        await _settle(tester);
        expect(
          wire.requests
              .where((r) => r['type'] == 'subscription.release.request')
              .single['subscriptionId'],
          'diff-1',
        );
        // The patch requested while shown arrives after the view hid.
        fullDiffs.single.complete({
          'cwd': '/repo',
          'files': [
            {'path': 'a.txt', 'hunks': <JsonMap>[]},
          ],
        });
        await _settle(tester);
        expect(git.patches, isEmpty);
        expect(git.loadingPatches, isEmpty);

        // Invalidation, a base ref push, a comparison change and a scrolled-in
        // patch all wait for the view to be shown.
        final hidden = wire.requests.length;
        CheckoutInvalidations.add('host', '/repo');
        wire.emit('checkout_status_update', {
          'cwd': '/repo',
          'isGit': true,
          'baseRef': 'develop',
        });
        WorkingDiffComparisons.instance.select(
          'host',
          'main',
          '/repo',
          'uncommitted',
        );
        unawaited(git.ensurePatch('a.txt'));
        await _settle(tester);
        expect(wire.requests.skip(hidden), isEmpty);
        expect(git.status?.text('baseRef'), 'develop');
        expect(
          git.files.single.text('path'),
          'a.txt',
          reason: 'the cached snapshot stays',
        );

        final revision = git.patchRevision;
        git.active = true;
        await _settle(tester);
        expect(wire.requests.skip(hidden).map((r) => r['type']), [
          'checkout_status_request',
          'subscribe_checkout_diff_request',
        ]);
        expect(jsonMap(wire.requests.last['compare'])['mode'], 'uncommitted');
        expect(git.files.single.text('path'), 'b.txt');
        expect(git.patchRevision, greaterThan(revision));
      } finally {
        git.dispose();
        WorkingDiffComparisons.instance.reset();
        for (final response in fullDiffs) {
          if (!response.isCompleted) {
            response.complete({'cwd': '/repo', 'files': <JsonMap>[]});
          }
        }
        await _settle(tester);
        client.dispose();
        await tester.runAsync(() => wire.close());
      }
    },
  );

  testWidgets(
    'a commit view hidden mid patch load remounts its placeholders when shown without reloading',
    (tester) async {
      final fileDiffs = <Completer<JsonMap>>[];
      final wire = _Wire(
        features: const {'ownedSubscriptions': true},
        respond: (request) {
          switch (request['type']) {
            case 'checkout_status_request':
              return {'cwd': '/repo', 'isGit': true};
            case 'checkout.commits.list.request':
              return {
                'commits': [
                  {
                    'sha': 'abc',
                    'files': [
                      {'path': 'a.txt'},
                    ],
                  },
                ],
              };
            case 'checkout.commits.file_diff.request':
              final response = Completer<JsonMap>();
              fileDiffs.add(response);
              return response.future;
          }
          return {};
        },
      );
      final client = await _connect(tester, wire);
      final git = GitController(
        client,
        const PanelTarget(
          kind: 'commit_diff',
          serverId: 'host',
          workspaceId: 'main',
          cwd: '/repo',
          id: 'abc',
          data: {'sha': 'abc'},
        ),
      );
      final file = {
        'path': 'a.txt',
        'status': 'modified',
        'hunks': <JsonMap>[],
      };
      try {
        unawaited(git.refresh());
        await _settle(tester);
        expect(git.files.single.text('path'), 'a.txt');
        unawaited(git.ensurePatch('a.txt'));
        await _settle(tester);
        expect(fileDiffs, hasLength(1));

        git.active = false;
        fileDiffs.single.complete({'file': file});
        await _settle(tester);
        expect(git.patches, isEmpty, reason: 'the late patch is dropped');

        final revision = git.patchRevision;
        final hidden = wire.requests.length;
        git.active = true;
        await _settle(tester);
        expect(
          git.patchRevision,
          greaterThan(revision),
          reason: 'its placeholder requests again',
        );
        expect(
          wire.requests.skip(hidden),
          isEmpty,
          reason: 'nothing changed while hidden',
        );

        unawaited(git.ensurePatch('a.txt'));
        await _settle(tester);
        expect(fileDiffs, hasLength(2));
        fileDiffs.last.complete({'file': file});
        await _settle(tester);
        expect(git.patches.keys, ['a.txt']);
      } finally {
        git.dispose();
        for (final response in fileDiffs) {
          if (!response.isCompleted) response.complete({'file': file});
        }
        await _settle(tester);
        client.dispose();
        await tester.runAsync(() => wire.close());
      }
    },
  );

  testWidgets(
    'a hidden working diff reloads its loaded commit list only after a change it missed',
    (tester) async {
      var lists = 0, subscriptions = 0;
      final wire = _Wire(
        features: const {'ownedSubscriptions': true, 'checkoutDiffAll': true},
        respond: (request) {
          switch (request['type']) {
            case 'checkout_status_request':
              return {'cwd': '/repo', 'isGit': true, 'baseRef': 'main'};
            case 'subscribe_checkout_diff_request':
              subscriptions++;
              return {
                'subscriptionId': 'diff-$subscriptions',
                'cwd': '/repo',
                'files': <JsonMap>[],
              };
            case 'checkout.commits.list.request':
              lists++;
              return {
                'commits': [
                  {'sha': 'c$lists'},
                ],
              };
          }
          return {};
        },
      );
      final client = await _connect(tester, wire);
      final git = GitController(client, _workingDiff);
      try {
        unawaited(git.refresh());
        unawaited(git.loadCommits());
        await _settle(tester);
        expect(git.commits.single.text('sha'), 'c1');

        // A tab switch alone resubscribes the diff but keeps the commit list.
        git.active = false;
        await _settle(tester);
        git.active = true;
        await _settle(tester);
        expect(wire.count('subscribe_checkout_diff_request'), 2);
        expect(lists, 1);

        git.active = false;
        await _settle(tester);
        wire.emit('checkout_status_update', {
          'cwd': '/repo',
          'isGit': true,
          'baseRef': 'main',
        });
        await _settle(tester);
        expect(lists, 1, reason: 'no commit reload while hidden');
        git.active = true;
        await _settle(tester);
        expect(lists, 2);
        expect(git.commits.single.text('sha'), 'c2');
      } finally {
        git.dispose();
        WorkingDiffComparisons.instance.reset();
        await _settle(tester);
        client.dispose();
        await tester.runAsync(() => wire.close());
      }
    },
  );

  testWidgets('a Changes view shares its checkout state status reads without losing invalidations, pushes or errors', (
    tester,
  ) async {
    final reads = <Completer<JsonMap>>[];
    var subscriptions = 0;
    final wire = _Wire(
      features: const {'ownedSubscriptions': true, 'checkoutDiffAll': true},
      respond: (request) {
        switch (request['type']) {
          case 'checkout_status_request':
            final read = Completer<JsonMap>();
            reads.add(read);
            return read.future;
          case 'subscribe_checkout_diff_request':
            subscriptions++;
            return {'subscriptionId': 'diff-$subscriptions', 'cwd': '/repo', 'files': <JsonMap>[]};
        }
        return {};
      },
    );
    final client = await _connect(tester, wire);
    final state = CheckoutGitState.acquire(client, 'host', '/repo');
    final git = GitController(client, _workingDiff, checkout: state);
    try {
      unawaited(git.refresh());
      await _settle(tester);
      expect(reads, hasLength(1), reason: "the view joins the state's initial read");

      // An invalidation while that read is in flight needs a status sent
      // after it, which both readers then share.
      CheckoutInvalidations.add('host', '/repo');
      await _settle(tester);
      expect(reads, hasLength(2));
      reads[0].complete({'cwd': '/repo', 'isGit': true, 'baseRef': 'main', 'currentBranch': 'before'});
      reads[1].complete({'cwd': '/repo', 'isGit': true, 'baseRef': 'main', 'currentBranch': 'after'});
      await _settle(tester);
      expect(state.status?.text('currentBranch'), 'after');
      expect(git.status?.text('currentBranch'), 'after');

      // A push after a read was sent wins over it; a later reader sends its own.
      CheckoutInvalidations.add('host', '/repo');
      await _settle(tester);
      expect(reads, hasLength(3));
      wire.emit('checkout_status_update', {
        'cwd': '/repo',
        'isGit': true,
        'baseRef': 'main',
        'currentBranch': 'pushed',
      });
      await _settle(tester);
      unawaited(git.refresh());
      await _settle(tester);
      expect(reads, hasLength(4));
      reads[2].complete({'cwd': '/repo', 'isGit': true, 'baseRef': 'main', 'currentBranch': 'stale'});
      reads[3].complete({'cwd': '/repo', 'isGit': true, 'baseRef': 'main', 'currentBranch': 'fresh'});
      await _settle(tester);
      expect(state.status?.text('currentBranch'), 'pushed');
      expect(git.status?.text('currentBranch'), 'fresh');

      // A failed shared read is the view's error and the state's payload.
      CheckoutInvalidations.add('host', '/repo');
      await _settle(tester);
      expect(reads, hasLength(5));
      reads[4].complete({
        'cwd': '/repo',
        'error': {'code': 'UNKNOWN', 'message': 'Not a repository'},
      });
      await _settle(tester);
      expect(git.error.toString(), contains('Not a repository'));
      expect(git.status?.text('currentBranch'), 'fresh');
      expect(checkoutPayloadError(state.status!), 'Not a repository');
    } finally {
      git.dispose();
      state.release();
      for (final read in reads) {
        if (!read.isCompleted) read.complete({'cwd': '/repo', 'isGit': true});
      }
      await _settle(tester);
      client.dispose();
      await tester.runAsync(() => wire.close());
    }
  });

  test('a replaced host client gets new shared checkout state while holders of the old one keep it', () {
    final first = PaseoClient(host: _host, autoReconnect: false);
    final second = PaseoClient(host: _host, autoReconnect: false);
    void listener() {}
    final header = CheckoutGitState.acquire(first, 'host', '/repo');
    final pane = CheckoutGitState.acquire(first, 'host', '/repo');
    expect(pane, same(header));
    header.status = {'cwd': '/repo', 'isGit': true, 'currentBranch': 'main'};

    final switched = CheckoutGitState.acquire(second, 'host', '/repo');
    expect(switched, isNot(same(header)));
    expect(switched.client, same(second));
    expect(
      switched.status,
      header.status,
      reason: 'cached status survives the switch',
    );

    header.release();
    // The pane still holds the previous client's state.
    expect(() => pane.addListener(listener), returnsNormally);
    pane.removeListener(listener);
    expect(CheckoutGitState.acquire(second, 'host', '/repo'), same(switched));

    pane.release();
    expect(() => pane.addListener(listener), throwsFlutterError);
    switched.release();
    expect(() => switched.addListener(listener), returnsNormally);
    switched.removeListener(listener);
    switched.release();
    expect(() => switched.addListener(listener), throwsFlutterError);
    first.dispose();
    second.dispose();
  });

  testWidgets(
    'a hidden pull request pane stops activity retries and pipeline polling until shown',
    (tester) async {
      SharedPreferences.setMockInitialValues({});
      tester.view.physicalSize = const Size(1440, 1000);
      tester.view.devicePixelRatio = 1;
      addTearDown(tester.view.reset);
      late PaseoLocalizations strings;
      await tester.runAsync(() async {
        strings = await PaseoLocalizations.load(const Locale('en'));
      });
      final wire = _Wire(
        features: const {
          'ownedSubscriptions': true,
          'forgeProviders': true,
          'forgeCheckDetails': true,
        },
        respond: (request) {
          switch (request['type']) {
            case 'checkout_status_request':
              return {'cwd': '/repo', 'isGit': true};
            case 'checkout_pr_status_request':
              return {
                'cwd': '/repo',
                'forge': 'gitlab',
                'githubFeaturesEnabled': true,
                'status': {
                  'number': 7,
                  'url': 'https://gitlab.example/o/r/-/merge_requests/7',
                  'title': 'Retained pane',
                  'state': 'open',
                  'repoOwner': 'o',
                  'repoName': 'r',
                  'forgeSpecific': {
                    'forge': 'gitlab',
                    'pipelineId': 99,
                    'pipelineStatus': 'running',
                  },
                },
              };
            case 'pull_request_timeline_request':
              return {'rpcError': 'Forge is rate limited'};
            case 'checkout.forge.get_check_details.request':
              return {
                'success': true,
                'details': {
                  'pipeline': {
                    'id': 99,
                    'status': 'running',
                    'stages': <JsonMap>[],
                  },
                },
              };
          }
          return {};
        },
      );
      final client = await _connect(tester, wire);
      final store = AppStore(connectHosts: false, discoverLocalHosts: false);
      store.hosts.add(
        HostState(config: client.host, client: client, serverId: 'host'),
      );
      final shown = ValueNotifier(true);
      try {
        await tester.pumpWidget(
          PaseoScope(
            store: store,
            child: MaterialApp(
              locale: const Locale('en'),
              localizationsDelegates: [_Loaded<PaseoLocalizations>(strings)],
              theme: paseoTheme(
                brightness: Brightness.dark,
                preferences: const {},
              ),
              home: Scaffold(
                body: ValueListenableBuilder<bool>(
                  valueListenable: shown,
                  builder: (context, visible, _) => Offstage(
                    offstage: !visible,
                    child: TickerMode(
                      enabled: visible,
                      child: PanelActivity(
                        visible: visible,
                        focused: visible,
                        appActive: true,
                        child: const PullRequestPanel(target: _pullRequest),
                      ),
                    ),
                  ),
                ),
              ),
            ),
          ),
        );
        await _settle(tester);
        expect(wire.count('pull_request_timeline_request'), 1);
        expect(wire.count('checkout.forge.get_check_details.request'), 1);

        // Hidden while the failed activity fetch waits for its retry and the
        // running pipeline is polled.
        shown.value = false;
        await tester.pump();
        for (var i = 0; i < 60; i++) {
          await tester.pump(const Duration(seconds: 1));
        }
        expect(wire.count('pull_request_timeline_request'), 1);
        expect(wire.count('checkout.forge.get_check_details.request'), 1);

        shown.value = true;
        await _settle(tester);
        expect(wire.count('pull_request_timeline_request'), 2);
        expect(wire.count('checkout.forge.get_check_details.request'), 2);
        await tester.pump(const Duration(seconds: 15));
        await _settle(tester);
        expect(wire.count('checkout.forge.get_check_details.request'), 3);
      } finally {
        await tester.pumpWidget(const SizedBox.shrink());
        await tester.pump(const Duration(seconds: 10));
        shown.dispose();
        await store.disposeAsync();
        client.dispose();
        await tester.runAsync(() => wire.close());
      }
    },
  );
}

class _Loaded<T> extends LocalizationsDelegate<T> {
  const _Loaded(this.value);
  final T value;
  @override
  bool isSupported(Locale locale) => true;
  @override
  Future<T> load(Locale locale) => SynchronousFuture(value);
  @override
  bool shouldReload(_Loaded<T> old) => false;
}

/// A daemon transport answering each request through [respond]; a response
/// carrying `rpcError` is sent as the request's `rpc_error`.
class _Wire implements DaemonTransport {
  _Wire({required this.features, required this.respond});
  final JsonMap features;
  final FutureOr<JsonMap> Function(JsonMap) respond;
  final requests = <JsonMap>[];
  final _messages = StreamController<Object>();
  @override
  Stream<Object> get messages => _messages.stream;

  int count(String type) => requests.where((r) => r['type'] == type).length;

  void emit(String type, JsonMap payload) {
    if (!_messages.isClosed) {
      _messages.add(
        jsonEncode({
          'type': 'session',
          'message': {'type': type, 'payload': payload},
        }),
      );
    }
  }

  @override
  void send(Object data) {
    final envelope = jsonMap(jsonDecode(data as String));
    if (envelope['type'] == 'hello') {
      emit('status', {
        'status': 'server_info',
        'serverId': 'host',
        'features': features,
      });
      return;
    }
    if (envelope['type'] == 'ping') {
      if (!_messages.isClosed) _messages.add(jsonEncode({'type': 'pong'}));
      return;
    }
    final request = jsonMap(envelope['message']);
    requests.add(request);
    final type = request['type'] as String;
    final responseType = type.endsWith('request')
        ? '${type.substring(0, type.length - 'request'.length)}response'
        : '${type}_response';
    Future<JsonMap>.sync(() => respond(request)).then((payload) {
      final error = payload['rpcError'];
      if (error is String) {
        emit('rpc_error', {
          'requestId': request['requestId'],
          'requestType': type,
          'error': error,
        });
        return;
      }
      emit(responseType, {...payload, 'requestId': request['requestId']});
    });
  }

  @override
  Future<void> close([int? code, String? reason]) async => _messages.close();
}
