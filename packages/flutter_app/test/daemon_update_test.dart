import 'package:flutter_test/flutter_test.dart';
import 'package:paseo_sessions/core/client.dart';
import 'package:paseo_sessions/core/json.dart';
import 'package:paseo_sessions/features/settings/daemon_update.dart';
import 'package:shared_preferences/shared_preferences.dart';

const _id = '0f8b6c5e-2a71-4c5b-9a43-7f2a1d0c9e11';

JsonMap _operation(String phase, {bool terminal = false, JsonMap? extra}) => {
  'operation': {
    'operationId': _id,
    'phase': phase,
    'terminal': terminal,
    'accepted': true,
    ...?extra,
  },
};

/// Replays scripted responses (a JsonMap or an error to throw) and records calls.
class _Daemon {
  _Daemon(this.script);
  final List<Object> script;
  final calls = <(String, JsonMap)>[];
  Future<JsonMap> request(
    String type,
    JsonMap fields,
    String responseType,
  ) async {
    calls.add((type, fields));
    expect(responseType, type.replaceFirst(RegExp(r'\.request$'), '.response'));
    final next = script.removeAt(0);
    if (next is JsonMap) return next;
    throw next;
  }
}

const _lost = DaemonException(
  'Daemon connection closed',
  code: 'DAEMON_CONNECTION_LOST',
);
const _offline = DaemonException(
  'Daemon is not connected',
  code: 'DAEMON_NOT_CONNECTED',
);

void main() {
  test(
    'a lost submit acknowledgement is retried with the same operation ID',
    () async {
      final daemon = _Daemon([_lost, _lost, _operation('admitted')]);
      final operation = await submitDaemonUpdate(
        daemon.request,
        _id,
        interval: Duration.zero,
      );
      expect(operation['phase'], 'admitted');
      expect(daemon.calls, hasLength(3));
      for (final (type, fields) in daemon.calls) {
        expect(type, 'daemon.update.submit.request');
        expect(fields, {'operationId': _id, 'source': 'release'});
      }
    },
  );

  test('a definitive submit rejection is not retried', () async {
    const rejected = DaemonException(
      'Different input for operation',
      code: 'OPERATION_CONFLICT',
    );
    final daemon = _Daemon([rejected, _operation('admitted')]);
    await expectLater(
      submitDaemonUpdate(daemon.request, _id, interval: Duration.zero),
      throwsA(same(rejected)),
    );
    expect(daemon.calls, hasLength(1));
  });

  test(
    'observation survives the daemon restart and only reads status',
    () async {
      final daemon = _Daemon([
        _operation('admitted'),
        _lost,
        _operation('draining'),
        _offline,
        _operation('verifying'),
        _operation(
          'committed',
          terminal: true,
          extra: {
            'candidate': {'version': '1.2.0'},
            'continuation': {
              'state': 'unknown',
              'agentId': 'agent',
              'messageId': 'message',
            },
          },
        ),
      ]);
      final phases = <Object?>[];
      final operation = await observeDaemonUpdate(
        daemon.request,
        _id,
        interval: Duration.zero,
        onUpdate: (operation) => phases.add(operation['phase']),
      );
      expect(phases, ['admitted', 'draining', 'verifying', 'committed']);
      expect(daemon.calls.map((call) => call.$1).toSet(), {
        'daemon.update.status.request',
      });
      expect(daemonUpdateOutcome(operation), DaemonUpdateOutcome.committed);
      // The update result stays committed; the resume is reported separately.
      expect(unconfirmedContinuation(operation), 'unknown');
    },
  );

  test('an answer for another operation is refused', () async {
    final daemon = _Daemon([
      {
        'operation': {
          'operationId': 'other',
          'phase': 'committed',
          'terminal': true,
        },
      },
    ]);
    await expectLater(
      observeDaemonUpdate(daemon.request, _id, interval: Duration.zero),
      throwsA(isA<StateError>()),
    );
  });

  test(
    'an unfinished observation reports the operation as still running',
    () async {
      final daemon = _Daemon([_operation('draining'), _operation('draining')]);
      await expectLater(
        observeDaemonUpdate(
          daemon.request,
          _id,
          interval: const Duration(milliseconds: 60),
          timeout: const Duration(milliseconds: 20),
        ),
        throwsA(
          isA<DaemonUpdateStillRunning>().having(
            (e) => e.operationId,
            'id',
            _id,
          ),
        ),
      );
    },
  );

  JsonMap committed(String? resume) => _operation(
    'committed',
    terminal: true,
    extra: {
      if (resume != null)
        'continuation': {'state': resume, 'agentId': 'agent', 'messageId': 'm'},
    },
  );

  test(
    'a pending session resume is observed until it settles, by status only',
    () async {
      final daemon = _Daemon([
        committed('pending'),
        _lost,
        committed('accepted'),
      ]);
      final operation = await observeDaemonUpdate(
        daemon.request,
        _id,
        interval: Duration.zero,
      );
      expect(unconfirmedContinuation(operation), isNull);
      expect(daemon.calls.map((call) => call.$1).toSet(), {
        'daemon.update.status.request',
      });
    },
  );

  test('a committed update is returned at the deadline even while its resume is pending', () async {
    final daemon = _Daemon([committed('pending'), committed('pending')]);
    final operation = await observeDaemonUpdate(
      daemon.request,
      _id,
      interval: const Duration(milliseconds: 60),
      timeout: const Duration(milliseconds: 20),
    );
    expect(daemonUpdateOutcome(operation), DaemonUpdateOutcome.committed);
    expect(unconfirmedContinuation(operation), 'pending');
  });

  test('an unsettled resume keeps the operation inspectable; settled results release it', () {
    expect(
      retainDaemonUpdateOperation(
        _operation('draining')['operation'] as JsonMap,
      ),
      isTrue,
    );
    for (final state in ['pending', 'unknown']) {
      expect(
        retainDaemonUpdateOperation(committed(state)['operation'] as JsonMap),
        isTrue,
        reason: state,
      );
    }
    for (final state in [null, 'accepted', 'not_delivered', 'failed']) {
      expect(
        retainDaemonUpdateOperation(committed(state)['operation'] as JsonMap),
        isFalse,
        reason: state,
      );
    }
    for (final phase in ['rolled_back', 'failed']) {
      expect(
        retainDaemonUpdateOperation(
          _operation(
                phase,
                terminal: true,
                extra: {
                  'continuation': {'state': 'unknown'},
                },
              )['operation']
              as JsonMap,
        ),
        isFalse,
        reason: phase,
      );
    }
  });

  test('outcomes and phase labels never present admission as completion', () {
    for (final phase in [
      'admitted',
      'preparing',
      'staged',
      'draining',
      'activating',
      'verifying',
      'rolling_back',
      'rolled_back',
      'failed',
    ]) {
      expect(
        daemonUpdatePhaseKey(phase),
        isNot('phaseComplete'),
        reason: phase,
      );
    }
    expect(daemonUpdatePhaseKey('committed'), 'phaseComplete');
    expect(
      daemonUpdateOutcome({'phase': 'committed', 'terminal': false}),
      DaemonUpdateOutcome.running,
    );
    expect(
      daemonUpdateOutcome({'phase': 'rolled_back', 'terminal': true}),
      DaemonUpdateOutcome.rolledBack,
    );
    expect(
      daemonUpdateOutcome({'phase': 'failed', 'terminal': true}),
      DaemonUpdateOutcome.failed,
    );
    expect(unconfirmedContinuation({'phase': 'committed'}), isNull);
    expect(
      unconfirmedContinuation({
        'continuation': {'state': 'accepted'},
      }),
      isNull,
    );
  });

  test('the capability gate and persisted operation identity', () async {
    expect(
      supportsDurableDaemonUpdates({
        'features': {'durableDaemonUpdates': true},
      }),
      isTrue,
    );
    expect(
      supportsDurableDaemonUpdates({
        'features': {'daemonSelfUpdate': true},
      }),
      isFalse,
    );
    SharedPreferences.setMockInitialValues({});
    const store = DaemonUpdateOperationStore();
    expect(await store.read('srv_a'), isNull);
    await store.write('srv_a', _id);
    expect(await store.read('srv_a'), _id);
    expect(await store.read('srv_b'), isNull);
    await store.clear('srv_a');
    expect(await store.read('srv_a'), isNull);
    expect(newDaemonUpdateOperationId(), isNot(newDaemonUpdateOperationId()));
  });
}
