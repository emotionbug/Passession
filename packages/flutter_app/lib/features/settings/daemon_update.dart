import 'dart:async';

import 'package:shared_preferences/shared_preferences.dart';
import 'package:uuid/uuid.dart';

import '../../core/client.dart';
import '../../core/json.dart';

/// Server capability for `daemon.update.submit` / `daemon.update.status`.
const durableDaemonUpdatesFeature = 'durableDaemonUpdates';

bool supportsDurableDaemonUpdates(JsonMap serverInfo) =>
    jsonMap(serverInfo['features'])[durableDaemonUpdatesFeature] == true;

/// Sends one daemon request and returns its response payload.
typedef DaemonUpdateRequest = Future<JsonMap> Function(
  String type,
  JsonMap fields,
  String responseType,
);

/// The operation's own result. A session resume is reported separately and never
/// changes it: a committed update stays committed when the resume is unconfirmed.
enum DaemonUpdateOutcome { running, committed, rolledBack, failed }

DaemonUpdateOutcome daemonUpdateOutcome(JsonMap operation) {
  if (operation['terminal'] != true) return DaemonUpdateOutcome.running;
  return switch (operation['phase']) {
    'committed' => DaemonUpdateOutcome.committed,
    'rolled_back' => DaemonUpdateOutcome.rolledBack,
    _ => DaemonUpdateOutcome.failed,
  };
}

/// `continuation.state` when a session resume was requested and is not accepted.
String? unconfirmedContinuation(JsonMap operation) {
  final continuation = operation['continuation'];
  if (continuation is! Map) return null;
  final state = stringValue(continuation['state'], 'unknown');
  return state == 'accepted' ? null : state;
}

/// Whether this app should keep the operation ID to inspect it again. A
/// running update and a committed one whose session resume is still `pending`
/// or `unknown` stay inspectable: a later status read can prove the receipt
/// accepted. Keeping the ID only re-reads status; it never resubmits.
bool retainDaemonUpdateOperation(JsonMap operation) {
  if (operation['terminal'] != true) return true;
  if (daemonUpdateOutcome(operation) != DaemonUpdateOutcome.committed)
    return false;
  return const {
    'pending',
    'unknown',
  }.contains(unconfirmedContinuation(operation));
}

/// Locale key suffix under `settings.host.daemon.update` for a durable phase.
/// `phaseComplete` is reserved for a committed operation.
String daemonUpdatePhaseKey(Object? phase) => switch (phase) {
  'admitted' => 'phaseStarting',
  'preparing' => 'phaseDownloading',
  'staged' => 'phaseInstalling',
  'draining' => 'phaseDraining',
  'activating' => 'phaseActivating',
  'verifying' => 'phaseVerifying',
  'rolling_back' || 'rolled_back' => 'phaseRollingBack',
  'committed' => 'phaseComplete',
  _ => 'phaseInstalling',
};

bool isDaemonUpdateReconnectFailure(Object error) =>
    error is DaemonException &&
    (error.outcomeUnknown ||
        const {
          'DAEMON_CONNECTION_LOST',
          'DAEMON_REQUEST_TIMEOUT',
          'DAEMON_NOT_CONNECTED',
        }.contains(error.code));

String newDaemonUpdateOperationId() => const Uuid().v4();

/// Observation deadline passed while the operation was still running; the
/// persisted operation ID lets a later view resume observing it.
class DaemonUpdateStillRunning implements Exception {
  const DaemonUpdateStillRunning(this.operationId, this.operation);
  final String operationId;
  final JsonMap? operation;
  @override
  String toString() => 'Update operation $operationId is still running.';
}

/// Remembers the operation a host is updating so a reconnect, rebuilt view or
/// app restart observes the same operation instead of starting another.
class DaemonUpdateOperationStore {
  const DaemonUpdateOperationStore();
  static String _key(String serverId) =>
      'paseo.daemonUpdate.operation.$serverId';
  Future<String?> read(String serverId) async =>
      (await SharedPreferences.getInstance()).getString(_key(serverId));
  Future<void> write(String serverId, String operationId) async {
    await (await SharedPreferences.getInstance()).setString(
      _key(serverId),
      operationId,
    );
  }

  Future<void> clear(String serverId) async {
    await (await SharedPreferences.getInstance()).remove(_key(serverId));
  }
}

JsonMap _operation(JsonMap response, String operationId) {
  final operation = jsonMap(response['operation']);
  if (stringValue(operation['operationId']) != operationId) {
    throw StateError('Daemon answered for a different update operation.');
  }
  return operation;
}

/// Submits an official release update. While the outcome is unknown (lost
/// acknowledgement, worker replaced mid-request) it resubmits the SAME
/// operation ID, which returns the admitted operation instead of a second update.
Future<JsonMap> submitDaemonUpdate(
  DaemonUpdateRequest request,
  String operationId, {
  Duration retryFor = const Duration(minutes: 2),
  Duration interval = const Duration(seconds: 1),
}) async {
  final deadline = DateTime.now().add(retryFor);
  while (true) {
    try {
      return _operation(
        await request('daemon.update.submit.request', {
          'operationId': operationId,
          'source': 'release',
        }, 'daemon.update.submit.response'),
        operationId,
      );
    } catch (error) {
      if (!isDaemonUpdateReconnectFailure(error) ||
          !DateTime.now().isBefore(deadline)) {
        rethrow;
      }
    }
    await Future<void>.delayed(interval);
  }
}

/// Polls the operation until it is terminal and any session resume has left
/// `pending`. Losing the connection is expected while the daemon is replaced
/// and only pauses observation; nothing here can cancel the update. At the
/// deadline a terminal result is returned as is; otherwise the operation is
/// reported as still running.
Future<JsonMap> observeDaemonUpdate(
  DaemonUpdateRequest request,
  String operationId, {
  void Function(JsonMap operation)? onUpdate,
  Duration interval = const Duration(seconds: 1),
  Duration timeout = const Duration(minutes: 30),
}) async {
  final deadline = DateTime.now().add(timeout);
  JsonMap? last;
  while (DateTime.now().isBefore(deadline)) {
    try {
      final operation = _operation(
        await request('daemon.update.status.request', {
          'operationId': operationId,
        }, 'daemon.update.status.response'),
        operationId,
      );
      last = operation;
      onUpdate?.call(operation);
      if (operation['terminal'] == true &&
          unconfirmedContinuation(operation) != 'pending') {
        return operation;
      }
    } catch (error) {
      if (!isDaemonUpdateReconnectFailure(error)) rethrow;
    }
    await Future<void>.delayed(interval);
  }
  if (last != null && last['terminal'] == true) return last;
  throw DaemonUpdateStillRunning(operationId, last);
}
