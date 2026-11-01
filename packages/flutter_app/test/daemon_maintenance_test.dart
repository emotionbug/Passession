import 'dart:async';
import 'dart:convert';

import 'package:flutter_test/flutter_test.dart';
import 'package:shared_preferences/shared_preferences.dart';

import 'package:paseo_sessions/core/app_store.dart';
import 'package:paseo_sessions/core/client.dart';
import 'package:paseo_sessions/core/daemon_maintenance.dart';
import 'package:paseo_sessions/core/host_config.dart';
import 'package:paseo_sessions/core/json.dart';

import 'fixtures/dispose_store.dart';

const _lan = HostConfig(
  id: 'lan',
  label: 'host',
  endpoint: 'ws://127.0.0.1:1/ws',
);

void main() {
  setUp(() => SharedPreferences.setMockInitialValues({}));

  test('a release only clears the operation it names', () {
    expect(
      applyDaemonMaintenanceEvent(null, {'operationId': 'a', 'active': true}),
      'a',
    );
    expect(
      applyDaemonMaintenanceEvent('b', {'operationId': 'a', 'active': false}),
      'b',
    );
    expect(
      applyDaemonMaintenanceEvent('a', {'operationId': 'a', 'active': false}),
      isNull,
    );
    expect(
      applyDaemonMaintenanceEvent('a', {'operationId': 'b', 'active': true}),
      'b',
    );
    for (final malformed in [
      <String, dynamic>{},
      {'operationId': '', 'active': true},
      {'operationId': 'b', 'active': 'yes'},
    ]) {
      expect(
        applyDaemonMaintenanceEvent('a', malformed),
        'a',
        reason: '$malformed',
      );
    }
  });

  test('server_info is a full snapshot of the pause', () {
    expect(
      daemonMaintenanceFromServerInfo({
        'daemonUpdate': {'operationId': 'a', 'active': true},
      }),
      'a',
    );
    expect(daemonMaintenanceFromServerInfo({'serverId': 'host'}), isNull);
    expect(
      daemonMaintenanceFromServerInfo({
        'daemonUpdate': {'operationId': 'a', 'active': false},
      }),
      isNull,
    );
  });

  testWidgets('a connected host shows and settles the daemon update pause', (
    tester,
  ) async {
    final daemon = _Daemon(durable: true, paused: 'op-1');
    final store = _store(daemon);
    try {
      await store.addHost(_lan);
      await tester.pump(const Duration(milliseconds: 50));
      final host = store.hosts.single;
      // Reconnect state comes from server_info, without waiting for an event.
      expect(host.maintenanceOperationId, 'op-1');
      final feed = daemon.requests.firstWhere(
        (r) => r['type'] == 'session.events.set_subscription.request',
      );
      expect(jsonList(feed['events']), contains('daemon.update.maintenance'));

      daemon.wire.emit('daemon.update.maintenance', {
        'operationId': 'op-0',
        'active': false,
      });
      await tester.pump();
      expect(
        host.maintenanceOperationId,
        'op-1',
        reason: 'stale release of an older operation',
      );

      daemon.wire.emit('daemon.update.maintenance', {
        'operationId': 'op-1',
        'active': false,
      });
      await tester.pump();
      expect(host.maintenanceOperationId, isNull);

      daemon.wire.emit('daemon.update.maintenance', {
        'operationId': 'op-2',
        'active': true,
      });
      await tester.pump();
      expect(host.maintenanceOperationId, 'op-2');

      daemon.wire.emit('status', daemon.serverInfo(paused: null));
      await tester.pump();
      expect(host.maintenanceOperationId, isNull);
    } finally {
      await disposeTestStore(tester, store);
    }
  });

  testWidgets('an older daemon is never asked for the maintenance event', (
    tester,
  ) async {
    final daemon = _Daemon(durable: false);
    final store = _store(daemon);
    try {
      await store.addHost(_lan);
      await tester.pump(const Duration(milliseconds: 50));
      final feed = daemon.requests.firstWhere(
        (r) => r['type'] == 'session.events.set_subscription.request',
      );
      expect(
        jsonList(feed['events']),
        isNot(contains('daemon.update.maintenance')),
      );
      expect(store.hosts.single.maintenanceOperationId, isNull);
    } finally {
      await disposeTestStore(tester, store);
    }
  });
}

AppStore _store(_Daemon daemon) => AppStore(
  discoverLocalHosts: false,
  clientId: 'maintenance-fixture',
  connector: (endpoint, {headers = const {}, protocols}) async =>
      daemon.connect(),
);

class _Daemon {
  _Daemon({required this.durable, this.paused});
  final bool durable;
  final String? paused;
  final wires = <_Wire>[];
  final requests = <JsonMap>[];
  int _subscriptions = 0;

  _Wire get wire => wires.last;

  DaemonTransport connect() {
    final wire = _Wire(this);
    wires.add(wire);
    return wire;
  }

  JsonMap serverInfo({required String? paused}) => {
    'status': 'server_info',
    'serverId': 'host',
    'features': {
      'ownedSubscriptions': true,
      'workspaceMultiplicity': true,
      if (durable) 'durableDaemonUpdates': true,
    },
    if (paused != null) 'daemonUpdate': {'operationId': paused, 'active': true},
  };

  JsonMap _reply(String type) {
    final sync = {'generation': 'g1', 'headSeq': 0};
    final page = {'hasMore': false};
    return switch (type) {
      'project.list.request' => {'projects': <Object>[], 'sync': sync},
      'fetch_workspaces_request' || 'fetch_agents_request' => {
        'subscriptionId': 'sub-${++_subscriptions}',
        'entries': <Object>[],
        'pageInfo': page,
        'sync': sync,
      },
      'session.events.set_subscription.request' => {
        'subscriptionId': 'sub-${++_subscriptions}',
      },
      _ => {},
    };
  }
}

class _Wire implements DaemonTransport {
  _Wire(this.daemon);
  final _Daemon daemon;
  final _messages = StreamController<Object>();

  @override
  Stream<Object> get messages => _messages.stream;

  void emit(String type, JsonMap payload) {
    if (_messages.isClosed) return;
    _messages.add(
      jsonEncode({
        'type': 'session',
        'message': {'type': type, 'payload': payload},
      }),
    );
  }

  @override
  void send(Object data) {
    if (_messages.isClosed) return;
    final envelope = jsonMap(jsonDecode(data as String));
    switch (envelope['type']) {
      case 'hello':
        emit('status', daemon.serverInfo(paused: daemon.paused));
      case 'ping':
        _messages.add(jsonEncode({'type': 'pong'}));
      case 'session':
        final request = jsonMap(envelope['message']);
        daemon.requests.add(request);
        final type = request['type'] as String;
        emit('${type.substring(0, type.length - 'request'.length)}response', {
          'requestId': request['requestId'],
          ...daemon._reply(type),
        });
    }
  }

  @override
  Future<void> close([int? code, String? reason]) async {
    if (!_messages.isClosed) await _messages.close();
  }
}
