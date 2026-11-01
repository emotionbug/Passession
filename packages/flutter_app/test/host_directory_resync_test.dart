import 'dart:async';
import 'dart:convert';

import 'package:flutter_test/flutter_test.dart';
import 'package:shared_preferences/shared_preferences.dart';

import 'package:paseo_sessions/core/app_store.dart';
import 'package:paseo_sessions/core/client.dart';
import 'package:paseo_sessions/core/host_config.dart';
import 'package:paseo_sessions/core/json.dart';

import 'fixtures/dispose_store.dart';

const _lan = HostConfig(
  id: 'lan',
  label: 'host',
  endpoint: 'ws://127.0.0.1:1/ws',
);

/// Host directory resync (AppStore.refreshHost): a resync requested while a
/// refresh runs is serviced after it, and the debounce is not postponed.
void main() {
  setUp(() => SharedPreferences.setMockInitialValues({}));

  testWidgets(
    'a generation mismatch during hydration refreshes again after it',
    (tester) async {
      final daemon = _Daemon()..workspaces = [_workspace('one')];
      final store = _store({'127.0.0.1': daemon});
      try {
        await store.addHost(_lan);
        final host = store.hosts.single;
        final agents = daemon.hold('fetch_agents_request');
        final refreshing = store.refreshHost('host');
        await tester.pump();
        expect(host.loadingLists, ['agent']);
        expect(daemon.count('fetch_workspaces_request'), 2);

        daemon
          ..generation = 'g2'
          ..workspaces = [_workspace('two')];
        daemon.update(_workspace('two'), generation: 'g2');
        // Past the debounce: the request must outlast the refresh it arrived in.
        await tester.pump(const Duration(milliseconds: 200));
        expect(host.workspaces.single['name'], 'one');
        expect(daemon.count('fetch_workspaces_request'), 2);

        agents.complete();
        await refreshing;
        expect(daemon.count('fetch_workspaces_request'), 3);
        expect(host.workspaces.single['name'], 'two');
        expect(host.listErrors, isEmpty);
      } finally {
        await disposeTestStore(tester, store);
      }
    },
  );

  testWidgets(
    'a feed restored while its own refresh runs refreshes again after it',
    (tester) async {
      final daemon = _Daemon(connectedHosts: true)..projects = [_project('p1')];
      final store = _store({'127.0.0.1': daemon});
      try {
        await store.addHost(_lan);
        final host = store.hosts.single;
        daemon.hold('project.list.request');
        final routes = daemon.hold('hosts.list.request');
        final refreshing = store.refreshHost('host');
        await tester.pump();
        expect(host.loadingLists, ['project']);

        // The project baseline fails with the connection; the refresh reconnects
        // for its route directory, which restores the feeds it just installed.
        await settleTestFuture(tester, daemon.wire.drop());
        await tester.pump(PaseoClient.reconnectDelay(0));
        expect(daemon.wires, hasLength(2));
        expect(host.listErrors.keys, ['project']);
        expect(daemon.count('project.list.request'), 2);
        expect(daemon.count('fetch_workspaces_request'), 3);

        daemon.projects = [_project('p2')];
        routes.complete();
        await refreshing;
        expect(daemon.count('project.list.request'), 3);
        expect(host.projects.single['projectId'], 'p2');
        expect(host.listErrors, isEmpty);
        expect(host.error, isNull);
      } finally {
        await disposeTestStore(tester, store);
      }
    },
  );

  testWidgets(
    'continuous mismatches refresh 150ms after the first instead of postponing it',
    (tester) async {
      final daemon = _Daemon()..workspaces = [_workspace('one')];
      final store = _store({'127.0.0.1': daemon});
      try {
        await store.addHost(_lan);
        final host = store.hosts.single;
        daemon
          ..generation = 'g2'
          ..workspaces = [_workspace('two')];
        daemon.update(_workspace('two'), generation: 'g2');
        await tester.pump(const Duration(milliseconds: 100));
        daemon.update(_workspace('two'), generation: 'g2', seq: 2);
        await tester.pump(const Duration(milliseconds: 60));
        expect(daemon.count('fetch_workspaces_request'), 2);

        await tester.pump(const Duration(milliseconds: 300));
        expect(host.workspaces.single['name'], 'two');
        // The burst shared one refresh.
        expect(daemon.count('fetch_workspaces_request'), 2);
      } finally {
        await disposeTestStore(tester, store);
      }
    },
  );

  testWidgets(
    'a resync requested on a replaced client is serviced only on its replacement',
    (tester) async {
      final first = _Daemon()..workspaces = [_workspace('first')];
      final second = _Daemon()
        ..generation = 'b1'
        ..workspaces = [_workspace('second')];
      final store = _store({'127.0.0.1': first, '127.0.0.2': second});
      try {
        await store.addHost(_lan);
        final host = store.hosts.single;
        first.hold('fetch_agents_request');
        final stale = store.refreshHost('host');
        await tester.pump();
        first.update(_workspace('first'), generation: 'g2');
        await tester.pump();

        await settleTestFuture(
          tester,
          store.updateHost(
            const HostConfig(
              id: 'lan',
              label: 'host',
              endpoint: 'ws://127.0.0.2:1/ws',
            ),
          ),
        );
        await settleTestFuture(tester, stale);
        await tester.pump(const Duration(milliseconds: 200));
        expect(first.count('project.list.request'), 2);
        expect(second.count('project.list.request'), 1);
        expect(host.workspaces.single['name'], 'second');
        expect(host.listErrors, isEmpty);

        // The replacement client still resyncs on its own mismatches.
        second
          ..generation = 'b2'
          ..workspaces = [_workspace('third')];
        second.update(_workspace('third'), generation: 'b2');
        await tester.pump(const Duration(milliseconds: 200));
        expect(second.count('project.list.request'), 2);
        expect(host.workspaces.single['name'], 'third');
        expect(first.count('project.list.request'), 2);
      } finally {
        await disposeTestStore(tester, store);
      }
    },
  );
}

JsonMap _workspace(String name) => {'id': 'w1', 'name': name};
JsonMap _project(String id) => {'projectId': id};

AppStore _store(Map<String, _Daemon> daemons) => AppStore(
  discoverLocalHosts: false,
  clientId: 'resync-fixture',
  connector: (endpoint, {headers = const {}, protocols}) async =>
      daemons[endpoint.host]!.connect(),
);

/// One daemon behind every connection opened to its endpoint. Directory
/// responses carry the daemon's state when they are sent.
class _Daemon {
  _Daemon({this.connectedHosts = false});
  final bool connectedHosts;
  final wires = <_Wire>[];
  final requests = <JsonMap>[];
  final _holds = <String, List<Completer<void>>>{};
  String generation = 'g1';
  List<JsonMap> projects = [];
  List<JsonMap> workspaces = [];
  int _subscriptions = 0;

  _Wire get wire => wires.last;
  int count(String type) => requests.where((r) => r['type'] == type).length;

  /// Withholds the response to the next [type] request until completed.
  Completer<void> hold(String type) {
    final gate = Completer<void>();
    _holds.putIfAbsent(type, () => []).add(gate);
    return gate;
  }

  Completer<void>? _take(String type) {
    final gates = _holds[type];
    return gates == null || gates.isEmpty ? null : gates.removeAt(0);
  }

  DaemonTransport connect() {
    final wire = _Wire(this);
    wires.add(wire);
    return wire;
  }

  void update(JsonMap workspace, {required String generation, int seq = 1}) =>
      wire.emit('workspace_update', {
        'kind': 'upsert',
        'workspace': workspace,
        'generation': generation,
        'seq': seq,
      });

  JsonMap _reply(String type) {
    final sync = {'generation': generation, 'headSeq': 0};
    final page = {'hasMore': false};
    return switch (type) {
      'project.list.request' => {'projects': projects, 'sync': sync},
      'fetch_workspaces_request' => {
        'subscriptionId': 'sub-${++_subscriptions}',
        'entries': workspaces,
        'pageInfo': page,
        'sync': sync,
      },
      'fetch_agents_request' => {
        'subscriptionId': 'sub-${++_subscriptions}',
        'entries': <Object>[],
        'pageInfo': page,
        'sync': sync,
      },
      'session.events.set_subscription.request' => {
        'subscriptionId': 'sub-${++_subscriptions}',
      },
      'hosts.list.request' => {
        'subscriptionId': 'sub-${++_subscriptions}',
        'directory': {'hosts': <Object>[]},
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

  /// The daemon end of this connection goes away.
  Future<void> drop() async => await _messages.close();

  @override
  void send(Object data) {
    if (_messages.isClosed) return;
    final envelope = jsonMap(jsonDecode(data as String));
    switch (envelope['type']) {
      case 'hello':
        emit('status', {
          'status': 'server_info',
          'serverId': 'host',
          'features': {
            'ownedSubscriptions': true,
            'workspaceMultiplicity': true,
            if (daemon.connectedHosts) 'connectedHosts': true,
          },
        });
      case 'ping':
        _messages.add(jsonEncode({'type': 'pong'}));
      case 'session':
        final request = jsonMap(envelope['message']);
        daemon.requests.add(request);
        final type = request['type'] as String;
        void respond() => emit(
          '${type.substring(0, type.length - 'request'.length)}response',
          {'requestId': request['requestId'], ...daemon._reply(type)},
        );
        final gate = daemon._take(type);
        if (gate == null) {
          respond();
        } else {
          unawaited(gate.future.then((_) => respond()));
        }
    }
  }

  @override
  Future<void> close([int? code, String? reason]) async {
    if (!_messages.isClosed) await _messages.close();
  }
}
