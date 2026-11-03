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
import 'package:paseo_sessions/features/files/file_panel.dart';
import 'package:paseo_sessions/features/files/remote_directory_browser.dart';
import 'package:paseo_sessions/ui/localization.dart';
import 'package:paseo_sessions/ui/panel_activity.dart';
import 'package:paseo_sessions/ui/theme.dart';

const _target = PanelTarget(
  kind: 'file',
  serverId: 'host',
  workspaceId: 'main',
  cwd: '/repo',
  id: 'a.txt',
  title: 'a.txt',
  data: {'path': 'a.txt'},
);

/// One host's files, shared by every connection to it.
class _Disk {
  final files = <String, ({String text, String revision})>{
    '/repo/a.txt': (text: 'one', revision: 'r1'),
    '/other/a.txt': (text: 'other copy', revision: 'o1'),
  };
  final directories = <String, List<JsonMap>>{
    '.': [
      {
        'name': 'src',
        'path': 'src',
        'kind': 'directory',
        'size': 0,
        'modifiedAt': '',
      },
    ],
    'src': [
      {
        'name': 'b.dart',
        'path': 'src/b.dart',
        'kind': 'file',
        'size': 1,
        'modifiedAt': '',
      },
    ],
  };
  int writes = 0;

  JsonMap _version(String key) {
    final file = files[key]!;
    return {
      'status': 'ready',
      'size': utf8.encode(file.text).length,
      'modifiedAt': 't-${file.revision}',
      'revision': file.revision,
      'kind': 'text',
      'mimeType': 'text/plain',
    };
  }

  JsonMap respond(JsonMap request) {
    final key = '${request['cwd']}/${request['path']}';
    switch (request['type']) {
      case 'fs.file.subscribe.request':
        return {
          'subscriptionId': 'sub-${request['requestId']}',
          'initial': _version(key),
        };
      case 'file_explorer_request':
        if (request['mode'] == 'list') {
          return {
            'directory': {
              'entries': directories[request['path']] ?? <JsonMap>[],
            },
          };
        }
        final file = files[key]!;
        return {
          'file': {
            'encoding': 'utf-8',
            'content': file.text,
            'size': utf8.encode(file.text).length,
            'mimeType': 'text/plain',
            'kind': 'text',
            'modifiedAt': 't-${file.revision}',
            'revision': file.revision,
          },
        };
      case 'fs.file.write.request':
        writes += 1;
        final content = request['content'] as String;
        files[key] = (text: content, revision: 'w$writes');
        return {
          'result': {
            'status': 'written',
            'size': utf8.encode(content).length,
            'modifiedAt': 't-w$writes',
            'revision': 'w$writes',
          },
        };
    }
    return {};
  }
}

/// One connection's transport; [connects] counts how often a client opened it.
class _Wire implements DaemonTransport {
  _Wire(this.disk);
  final _Disk disk;
  int connects = 0;
  final requests = <JsonMap>[];
  final _messages = StreamController<Object>();
  @override
  Stream<Object> get messages => _messages.stream;

  List<JsonMap> ofType(String type) =>
      requests.where((r) => r['type'] == type).toList();

  void _emit(String type, JsonMap payload) {
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
    final envelope = jsonMap(jsonDecode(data as String));
    if (envelope['type'] == 'hello') {
      _emit('status', {
        'status': 'server_info',
        'serverId': 'host',
        'features': {'ownedSubscriptions': true, 'workspaceFileEditing': true},
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
    final payload = disk.respond(request);
    scheduleMicrotask(
      () =>
          _emit(responseType, {...payload, 'requestId': request['requestId']}),
    );
  }

  @override
  Future<void> close([int? code, String? reason]) async {
    // Never await `done`: a wire whose client never connected has no listener,
    // and its done future would wait forever for one.
    if (!_messages.isClosed) unawaited(_messages.close());
  }
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

void main() {
  late PaseoLocalizations strings;

  /// Pumps [count] frames, letting the real event loop run after each. The file
  /// service awaits stream-subscription cancels, and those futures complete in
  /// the root zone, whose microtasks fake-async pumps never run: a file read
  /// would otherwise never finish.
  Future<void> frames(WidgetTester tester, [int count = 20]) async {
    for (var i = 0; i < count; i++) {
      await tester.pump(const Duration(milliseconds: 16));
      await tester.runAsync(() async {});
    }
  }

  PaseoClient client(_Wire wire, String id) => PaseoClient(
    host: const HostConfig(
      id: 'host',
      label: 'host',
      endpoint: 'ws://127.0.0.1:1/ws',
    ),
    clientId: id,
    autoReconnect: false,
    connector: (endpoint, {headers = const {}, protocols}) async {
      wire.connects += 1;
      return wire;
    },
  );

  Future<PaseoClient> connect(WidgetTester tester, PaseoClient client) async {
    final connecting = client.connect();
    await frames(tester, 5);
    await connecting;
    return client;
  }

  Widget app(AppStore store, Widget child) => PaseoScope(
    store: store,
    child: MaterialApp(
      locale: const Locale('en'),
      localizationsDelegates: [_Loaded<PaseoLocalizations>(strings)],
      theme: paseoTheme(brightness: Brightness.dark, preferences: const {}),
      home: Scaffold(body: child),
    ),
  );

  setUp(() {
    SharedPreferences.setMockInitialValues({});
  });

  testWidgets(
    'a file tab keeps its buffer across hidden, connection and cwd changes, writing only its own file',
    (tester) async {
      tester.view.physicalSize = const Size(1200, 900);
      tester.view.devicePixelRatio = 1;
      addTearDown(tester.view.reset);
      await tester.runAsync(() async {
        strings = await PaseoLocalizations.load(const Locale('en'));
      });
      final disk = _Disk();
      final first = _Wire(disk), second = _Wire(disk);
      final firstClient = await connect(
        tester,
        client(first, 'file-ownership-first'),
      );
      // The store swaps a connection in before it connects.
      final secondClient = client(second, 'file-ownership-second');
      final store = AppStore(connectHosts: false, discoverLocalHosts: false);
      final host = HostState(
        config: firstClient.host,
        client: firstClient,
        serverId: 'host',
      );
      store.hosts.add(host);
      final state = ValueNotifier((visible: false, target: _target));
      String editorText() => tester
          .widget<EditableText>(find.byType(EditableText))
          .controller
          .text;

      List<JsonMap> writes(_Wire wire) => wire.ofType('fs.file.write.request');
      try {
        await tester.pumpWidget(
          app(
            store,
            ValueListenableBuilder(
              valueListenable: state,
              builder: (context, value, _) => PanelActivity(
                visible: value.visible,
                focused: value.visible,
                appActive: true,
                child: Offstage(
                  offstage: !value.visible,
                  child: FilePanel(target: value.target),
                ),
              ),
            ),
          ),
        );
        await frames(tester);
        expect(
          first.ofType('fs.file.subscribe.request'),
          isEmpty,
          reason: 'a hidden file tab does not observe its file',
        );

        state.value = (visible: true, target: _target);
        await frames(tester);
        expect(
          first.ofType('fs.file.subscribe.request').single['cwd'],
          '/repo',
        );
        expect(editorText(), 'one');

        await tester.enterText(find.byType(EditableText), 'one edited');
        await frames(tester, 3);

        // The store switches the host to another connection: the old client
        // closes and the host holds the new one before it has connected.
        final closing = firstClient.close();
        await frames(tester, 3);
        await closing;
        host.client = secondClient;
        // The shell rebuilds its panels on the store change that follows.
        state.value = (visible: true, target: _target.copyWith());
        await frames(tester, 5);
        expect(
          second.ofType('fs.file.subscribe.request'),
          isEmpty,
          reason: 'observing waits for the new connection to come up',
        );
        expect(
          editorText(),
          'one edited',
          reason: 'the unsaved buffer stays on screen meanwhile',
        );

        await connect(tester, secondClient);
        state.value = (visible: true, target: _target.copyWith());
        await frames(tester, 5);
        expect(
          second.ofType('fs.file.subscribe.request').single['cwd'],
          '/repo',
          reason: 'the connection coming up starts observing exactly once',
        );
        expect(
          first.connects,
          1,
          reason: 'the closed client must not be reconnected',
        );
        expect(
          editorText(),
          'one edited',
          reason: 'the unsaved buffer survives the switch',
        );

        // Autosave writes through the new connection over the version read.
        await tester.pump(const Duration(seconds: 1));
        await frames(tester);
        expect(writes(first), isEmpty);
        expect(writes(second).single, containsPair('content', 'one edited'));
        expect(writes(second).single, containsPair('expectedRevision', 'r1'));

        // The same tab key reopened under another cwd names another file. The
        // unsaved edit goes to the file it was made in, never to the new one.
        await tester.enterText(find.byType(EditableText), 'one edited again');
        await frames(tester, 5);
        state.value = (visible: true, target: _target.copyWith(cwd: '/other'));
        await frames(tester);
        expect(writes(second), hasLength(2));
        expect(writes(second).last, containsPair('cwd', '/repo'));
        expect(
          writes(second).last,
          containsPair('content', 'one edited again'),
        );
        expect(disk.files['/repo/a.txt']!.text, 'one edited again');
        expect(disk.files['/other/a.txt']!.text, 'other copy');
        expect(editorText(), 'other copy');
        expect(
          second.ofType('fs.file.subscribe.request').map((r) => r['cwd']),
          ['/repo', '/other'],
        );
      } finally {
        await tester.pumpWidget(const SizedBox.shrink());
        state.dispose();
        await store.disposeAsync();
        firstClient.dispose();
        secondClient.dispose();
        await tester.runAsync(() async {
          await first.close();
          await second.close();
        });
      }
    },
    timeout: const Timeout(Duration(minutes: 2)),
  );

  testWidgets(
    'the explorer keeps its expanded folders when its host switches connections',
    (tester) async {
      await tester.runAsync(() async {
        strings = await PaseoLocalizations.load(const Locale('en'));
      });
      final disk = _Disk();
      final first = _Wire(disk), second = _Wire(disk);
      final firstClient = await connect(
        tester,
        client(first, 'explorer-ownership-first'),
      );
      // The store swaps a connection in before it connects.
      final secondClient = client(second, 'explorer-ownership-second');
      final store = AppStore(connectHosts: false, discoverLocalHosts: false);
      final values = <String, dynamic>{
        'expandedPathsByWorkspace': {
          'workspace:main': ['.', 'src'],
        },
      };
      final connection = ValueNotifier(firstClient);
      List<String> listed(_Wire wire) => wire
          .ofType('file_explorer_request')
          .map((r) => r['path'] as String)
          .toList();
      try {
        await tester.pumpWidget(
          app(
            store,
            ValueListenableBuilder(
              valueListenable: connection,
              builder: (context, current, _) => RemoteDirectoryBrowser(
                client: current,
                cwd: '/repo',
                navigationMenu: false,
                preferences: ExplorerPreferences(
                  values: values,
                  save: () async {},
                  workspaceKey: 'workspace:main',
                ),
              ),
            ),
          ),
        );
        await frames(tester);
        expect(find.text('b.dart'), findsOneWidget);
        expect(listed(first), containsAll(['.', 'src']));

        final closing = firstClient.close();
        await frames(tester, 5);
        await closing;
        connection.value = secondClient;
        await frames(tester);
        expect(
          listed(second),
          isEmpty,
          reason: 'listing waits for the new connection',
        );
        expect(
          find.text('b.dart'),
          findsOneWidget,
          reason: 'the tree stays meanwhile',
        );

        await connect(tester, secondClient);
        await frames(tester);
        expect(listed(second), containsAll(['.', 'src']));
        expect(first.connects, 1);
        expect(
          find.text('b.dart'),
          findsOneWidget,
          reason: 'the expanded folder stays expanded',
        );
        expect(jsonMap(values['expandedPathsByWorkspace'])['workspace:main'], [
          '.',
          'src',
        ]);
      } finally {
        await tester.pumpWidget(const SizedBox.shrink());
        connection.dispose();
        await store.disposeAsync();
        firstClient.dispose();
        secondClient.dispose();
        await tester.runAsync(() async {
          await first.close();
          await second.close();
        });
      }
    },
    timeout: const Timeout(Duration(minutes: 2)),
  );
}
