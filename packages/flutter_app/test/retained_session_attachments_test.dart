import 'dart:async';
import 'dart:convert';

import 'package:desktop_drop/desktop_drop.dart';
import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:shared_preferences/shared_preferences.dart';

import 'package:paseo_sessions/core/app_store.dart';
import 'package:paseo_sessions/core/client.dart';
import 'package:paseo_sessions/core/host_config.dart';
import 'package:paseo_sessions/core/json.dart';
import 'package:paseo_sessions/core/scope.dart';
import 'package:paseo_sessions/core/target.dart';
import 'package:paseo_sessions/features/chat/agent_panel.dart';
import 'package:paseo_sessions/features/chat/attachment_pill.dart';
import 'package:paseo_sessions/features/chat/chat_drafts.dart';
import 'package:paseo_sessions/ui/localization.dart';
import 'package:paseo_sessions/ui/panel_activity.dart';
import 'package:paseo_sessions/ui/theme.dart';

const _first = 'a5a5a5a5-0000-4000-8000-000000000051';
const _second = 'a5a5a5a5-0000-4000-8000-000000000052';
const _png =
    'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==';

PanelTarget _target(String id) => PanelTarget(
  kind: 'agent',
  serverId: 'host',
  workspaceId: 'main',
  cwd: '/project',
  id: id,
);

JsonMap _agent(String id) => {
  'id': id,
  'provider': 'codex',
  'cwd': '/project',
  'workspaceId': 'main',
  'title': id == _first ? 'First session' : 'Second session',
  'status': 'idle',
  'pendingPermissions': <JsonMap>[],
};

Finder _panel(String id) => find.byKey(ValueKey('panel:$id'));
Finder _editor(String id) => find.descendant(
  of: _panel(id),
  matching: find.byWidgetPredicate((w) => w is EditableText && !w.readOnly),
);

void main() {
  // One widget test keeps the singleton draft store and its persistence futures
  // in the same fake-async zone across all retained-panel transitions.
  testWidgets(
    'dropped images stay with the visible recipient across retained tabs and split panes',
    (tester) async {
      SharedPreferences.setMockInitialValues({});
      debugDefaultTargetPlatformOverride = TargetPlatform.linux;
      tester.view.physicalSize = const Size(1440, 1000);
      tester.view.devicePixelRatio = 1;
      addTearDown(tester.view.reset);
      late PaseoLocalizations strings;
      await tester.runAsync(() async {
        strings = await PaseoLocalizations.load(const Locale('en'));
      });
      Completer<JsonMap>? pendingSend;
      final wire = _Wire((request) {
        final id = stringValue(request['agentId']);
        switch (request['type']) {
          case 'agent.timeline.set_subscription.request':
            return {
              'subscriptionId':
                  'timeline-${jsonList(request['agentIds']).single}',
            };
          case 'agent.timeline.list_prompts.request':
            return {'epoch': 'epoch-$id', 'prompts': <JsonMap>[]};
          case 'fetch_agent_timeline_request':
            return {
              'agentId': id,
              'agent': _agent(id),
              'direction': request['direction'],
              'projection': 'projected',
              'epoch': 'epoch-$id',
              'window': {'minSeq': 1, 'maxSeq': 1, 'nextSeq': 2},
              'reset': false,
              'staleCursor': false,
              'gap': false,
              'hasOlder': false,
              'hasNewer': false,
              'startCursor': {'epoch': 'epoch-$id', 'seq': 1},
              'endCursor': {'epoch': 'epoch-$id', 'seq': 1},
              'entries': [
                {
                  'seq': 1,
                  'seqStart': 1,
                  'seqEnd': 1,
                  'turnId': 'saved-$id',
                  'timestamp': '2026-05-01T09:00:00.000Z',
                  'item': {'type': 'assistant_message', 'text': 'Ready $id'},
                },
              ],
              'historySource': 'live',
            };
          case 'send_agent_message_request':
            return pendingSend?.future ?? {'agentId': id, 'accepted': true};
        }
        return {};
      });
      final client = PaseoClient(
        host: const HostConfig(
          id: 'host',
          label: 'host',
          endpoint: 'ws://127.0.0.1:1/ws',
        ),
        clientId: 'retained-attachments-regression',
        autoReconnect: false,
        connector: (endpoint, {headers = const {}, protocols}) async => wire,
      );
      final connecting = client.connect();
      for (var i = 0; i < 5; i++) {
        await tester.pump(const Duration(milliseconds: 10));
      }
      await connecting;
      final store = AppStore(connectHosts: false, discoverLocalHosts: false);
      store.hosts.add(
        HostState(config: client.host, client: client, serverId: 'host'),
      );
      final presentation = ValueNotifier((selected: _first, split: false));
      final drafts = ChatDrafts.instance;
      String key(String id) => ChatDrafts.key('host', 'main', id);
      List<JsonMap> images(String id) =>
          jsonList(drafts.draft(key(id))['images']).map(jsonMap).toList();
      List<JsonMap> sends() => wire.requests
          .where((r) => r['type'] == 'send_agent_message_request')
          .toList();
      Future<void> frames([int count = 20]) async {
        for (var i = 0; i < count; i++) {
          await tester.pump(const Duration(milliseconds: 16));
        }
      }

      Future<void> drop(String id) async {
        final target = find.descendant(
          of: _panel(id),
          matching: find.byType(DropTarget),
        );
        final point = tester.getCenter(target);
        final messenger = tester.binding.defaultBinaryMessenger;
        const codec = StandardMethodCodec();
        await messenger.handlePlatformMessage(
          'desktop_drop',
          codec.encodeMethodCall(MethodCall('entered', [point.dx, point.dy])),
          (_) {},
        );
        // The plugin's byte-backed event avoids filesystem timing while still
        // broadcasting through its real channel, bounds and listener registry.
        await messenger.handlePlatformMessage(
          'desktop_drop',
          codec.encodeMethodCall(
            MethodCall('performOperation_web', [
              {
                'uri': '/fixture/screenshot.png',
                'name': 'screenshot.png',
                'type': 'image/png',
                'size': base64Decode(_png).length,
                'data': base64Decode(_png),
                'lastModified': 0,
              },
            ]),
          ),
          (_) {},
        );
        await frames();
      }

      Future<void> send(String id, String text) async {
        await tester.tap(_editor(id));
        await tester.enterText(_editor(id), text);
        await tester.sendKeyEvent(LogicalKeyboardKey.enter);
        await frames();
      }

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
                body: ValueListenableBuilder(
                  valueListenable: presentation,
                  builder: (context, state, _) => LayoutBuilder(
                    builder: (context, constraints) => Stack(
                      children: [
                        for (final id in [_first, _second])
                          Positioned(
                            left: state.split && id == _second
                                ? constraints.maxWidth / 2
                                : 0,
                            right: state.split && id == _first
                                ? constraints.maxWidth / 2
                                : 0,
                            top: 0,
                            bottom: 0,
                            child: Offstage(
                              offstage: !state.split && state.selected != id,
                              child: TickerMode(
                                enabled: state.split || state.selected == id,
                                child: ExcludeFocus(
                                  excluding:
                                      !state.split && state.selected != id,
                                  child: IgnorePointer(
                                    ignoring:
                                        !state.split && state.selected != id,
                                    child: PanelActivity(
                                      visible:
                                          state.split || state.selected == id,
                                      focused: state.selected == id,
                                      appActive: true,
                                      child: AgentPanel(
                                        key: ValueKey('panel:$id'),
                                        target: _target(id),
                                      ),
                                    ),
                                  ),
                                ),
                              ),
                            ),
                          ),
                      ],
                    ),
                  ),
                ),
              ),
            ),
          ),
        );
        await frames();
        drafts.update(key(_second), {'text': 'Second session draft'});
        await frames();
        await drop(_first);
        expect(images(_first), [
          {'data': _png, 'mimeType': 'image/png'},
        ]);
        expect(
          images(_second),
          isEmpty,
          reason: 'an offstage session must never receive another session\'s image drop',
        );
        expect(find.byType(AttachmentPill), findsOneWidget);
        await send(_first, 'Analyze this screenshot');
        expect(sends().single['agentId'], _first);
        expect(sends().single['images'], [
          {'data': _png, 'mimeType': 'image/png'},
        ]);
        expect(images(_first), isEmpty);

        presentation.value = (selected: _second, split: false);
        await frames();
        expect(
          tester.widget<EditableText>(_editor(_second)).controller.text,
          'Second session draft',
        );
        expect(find.byType(AttachmentPill), findsNothing);
        await send(_second, 'An unrelated text-only conversation');
        expect(sends().last['agentId'], _second);
        expect(jsonList(sends().last['images']), isEmpty);

        // A visible, unfocused split pane remains a valid drop recipient. Focus
        // must not redirect its image into the other visible session.
        presentation.value = (selected: _first, split: true);
        await frames();
        await drop(_second);
        expect(images(_first), isEmpty);
        expect(images(_second), [
          {'data': _png, 'mimeType': 'image/png'},
        ]);
        pendingSend = Completer<JsonMap>();
        await send(_second, 'Only the right pane owns this image');
        expect(sends(), hasLength(3));
        expect(sends().last['agentId'], _second);
        expect(sends().last['images'], [
          {'data': _png, 'mimeType': 'image/png'},
        ]);
        await drop(_second);
        expect(images(_first), isEmpty);
        expect(
          images(_second),
          isEmpty,
          reason: 'a sending composer must not accept a new drop',
        );
        pendingSend.complete({'agentId': _second, 'accepted': true});
        pendingSend = null;
        await frames();

        presentation.value = (selected: _first, split: false);
        await frames();
        await drop(_first);
        expect(images(_first), [
          {'data': _png, 'mimeType': 'image/png'},
        ]);
        expect(
          images(_second),
          isEmpty,
          reason: 'switching visibility must reassign drop subscriptions',
        );

        // Explorer actions do not navigate. A workspace inbox update must
        // reach the already-selected chat without an unrelated store rebuild.
        store.open(_target(_first));
        await frames();
        const file = {
          'kind': 'workspace_file',
          'path': '/project/notes.md',
          'selection': {'kind': 'whole_file'},
        };
        drafts.addAttachments('host', 'another-workspace', [file]);
        drafts.addAttachments('host', 'main', [file]);
        await frames();
        expect(jsonList(drafts.draft(key(_first))['attachments']), [file]);
        expect(jsonList(drafts.draft(key(_second))['attachments']), isEmpty);
        expect(find.textContaining('notes.md'), findsWidgets);
      } finally {
        if (pendingSend != null && !pendingSend.isCompleted) {
          pendingSend.complete({'agentId': _second, 'accepted': true});
          await frames();
        }
        await tester.pumpWidget(const SizedBox.shrink());
        presentation.dispose();
        await store.disposeAsync();
        client.dispose();
        await tester.runAsync(() => wire.close());
        debugDefaultTargetPlatformOverride = null;
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

class _Wire implements DaemonTransport {
  _Wire(this.respond);
  final FutureOr<JsonMap> Function(JsonMap) respond;
  final requests = <JsonMap>[];
  final _messages = StreamController<Object>();
  @override
  Stream<Object> get messages => _messages.stream;
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
        'features': {'ownedSubscriptions': true},
      });
      return;
    }
    if (envelope['type'] == 'ping') {
      _messages.add(jsonEncode({'type': 'pong'}));
      return;
    }
    final request = jsonMap(envelope['message']);
    requests.add(request);
    final type = request['type'] as String;
    final responseType = type.endsWith('request')
        ? '${type.substring(0, type.length - 'request'.length)}response'
        : '${type}_response';
    Future<JsonMap>.sync(() => respond(request)).then((payload) {
      emit(responseType, {...payload, 'requestId': request['requestId']});
    });
  }

  @override
  Future<void> close([int? code, String? reason]) async => _messages.close();
}
