import 'dart:async';
import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:paseo_sessions/core/app_store.dart';
import 'package:paseo_sessions/core/client.dart';
import 'package:paseo_sessions/core/host_config.dart';
import 'package:paseo_sessions/core/json.dart';
import 'package:paseo_sessions/core/scope.dart';
import 'package:paseo_sessions/core/target.dart';
import 'package:paseo_sessions/ui/localization.dart';
import 'package:paseo_sessions/ui/shell.dart';
import 'package:paseo_sessions/ui/theme.dart';

class TabCloseFixture {
  TabCloseFixture() {
    store = AppStore(
      discoverLocalHosts: false,
      clientId: 'tab-fixture',
      connector: (_, {headers = const {}, protocols}) async => _Wire(this),
    );
  }
  late final AppStore store;
  final requests = <JsonMap>[];
  final agents = <JsonMap>[
    {
      'id': 'root-one',
      'title': 'Running conversation',
      'provider': 'omp',
      'workspaceId': 'workspace',
      'cwd': '/fixture',
      'status': 'running',
      'lifecycle': 'running',
      'persistence': {'sessionId': 'native-root-one'},
      'labels': <String, dynamic>{},
    },
    {
      'id': 'root-two',
      'title': 'Other conversation',
      'provider': 'omp',
      'workspaceId': 'workspace',
      'cwd': '/fixture',
      'status': 'idle',
      'lifecycle': 'idle',
      'persistence': {'sessionId': 'native-root-two'},
      'labels': <String, dynamic>{},
    },
    {
      'id': 'child-one',
      'title': 'Pinned child',
      'provider': 'omp',
      'workspaceId': 'workspace',
      'cwd': '/fixture',
      'parentAgentId': 'root-one',
      'status': 'idle',
      'lifecycle': 'idle',
      'persistence': {'sessionId': 'native-child'},
      'labels': <String, dynamic>{},
    },
  ];
  PanelTarget target(String id) => PanelTarget(
    kind: 'agent',
    serverId: 'host',
    workspaceId: 'workspace',
    cwd: '/fixture',
    id: id,
    title: agents.firstWhere((agent) => agent['id'] == id)['title'] as String,
  );

  Future<void> initialize() async {
    await store.addHost(
      const HostConfig(
        id: 'fixture',
        label: 'Fixture',
        endpoint: 'ws://127.0.0.1:1/ws',
      ),
    );
    store.sidebarVisible = false;
    for (final id in ['root-one', 'root-two', 'child-one']) {
      store.open(target(id));
    }
    // Exercise tab chrome without starting chat subscriptions or provider turns.
    store.focusedPane.selectedKey = null;
  }

  Widget app() => PaseoScope(
    store: store,
    child: MaterialApp(
      locale: const Locale('en'),
      supportedLocales: supportedPaseoLocales,
      localizationsDelegates: const [PaseoLocalizations.delegate],
      theme: paseoTheme(brightness: Brightness.dark, preferences: {}),
      home: const PaseoShell(),
    ),
  );

  JsonMap reply(JsonMap request) {
    final sync = {'generation': 'fixture-generation', 'headSeq': 0};
    return switch (request['type']) {
      'project.list.request' => {'projects': <Object>[], 'sync': sync},
      'fetch_workspaces_request' => {
        'subscriptionId': 'workspaces',
        'entries': [
          {'id': 'workspace', 'name': 'Fixture', 'cwd': '/fixture'},
        ],
        'pageInfo': {'hasMore': false},
        'sync': sync,
      },
      'fetch_agents_request' => {
        'subscriptionId': 'agents',
        'entries': agents,
        'pageInfo': {'hasMore': false},
        'sync': sync,
      },
      'session.events.set_subscription.request' => {'subscriptionId': 'events'},
      'update_agent_request' => {
        'agent': agents.firstWhere(
          (agent) => agent['id'] == request['agentId'],
        ),
      },
      _ => <String, dynamic>{},
    };
  }
}

class _Wire implements DaemonTransport {
  _Wire(this.fixture);
  final TabCloseFixture fixture;
  final _messages = StreamController<Object>();
  @override
  Stream<Object> get messages => _messages.stream;
  void emit(String type, JsonMap payload) {
    if (!_messages.isClosed)
      _messages.add(
        jsonEncode({
          'type': 'session',
          'message': {'type': type, 'payload': payload},
        }),
      );
  }

  @override
  void send(Object data) {
    final envelope = jsonMap(jsonDecode(data as String));
    switch (envelope['type']) {
      case 'hello':
        emit('status', {
          'status': 'server_info',
          'serverId': 'host',
          'features': {
            'ownedSubscriptions': true,
            'workspaceMultiplicity': true,
          },
        });
      case 'ping':
        _messages.add(jsonEncode({'type': 'pong'}));
      case 'session':
        final request = jsonMap(envelope['message']);
        fixture.requests.add(request);
        final type = request['type'] as String;
        if (type.endsWith('request')) {
          emit('${type.substring(0, type.length - 'request'.length)}response', {
            'requestId': request['requestId'],
            ...fixture.reply(request),
          });
        }
    }
  }

  @override
  Future<void> close([int? code, String? reason]) async {
    if (!_messages.isClosed) await _messages.close();
  }
}
