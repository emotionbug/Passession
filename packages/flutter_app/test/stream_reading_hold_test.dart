import 'dart:async';
import 'dart:convert';

import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:paseo_sessions/core/app_store.dart';
import 'package:paseo_sessions/core/client.dart';
import 'package:paseo_sessions/core/host_config.dart';
import 'package:paseo_sessions/core/json.dart';
import 'package:paseo_sessions/core/scope.dart';
import 'package:paseo_sessions/core/target.dart';
import 'package:paseo_sessions/features/chat/agent_panel.dart';
import 'package:paseo_sessions/features/chat/chat_drafts.dart';
import 'package:paseo_sessions/features/chat/permission_card.dart';
import 'package:paseo_sessions/ui/localization.dart';
import 'package:paseo_sessions/ui/panel_activity.dart';
import 'package:paseo_sessions/ui/theme.dart';
import 'package:shared_preferences/shared_preferences.dart';

const _agentId = 'a0000000-0000-4000-8000-000000000001';
const _target = PanelTarget(
  kind: 'agent',
  serverId: 'host',
  workspaceId: 'main',
  cwd: '/project',
  id: _agentId,
);

JsonMap get _runningAgent => {
  'id': _agentId,
  'workspaceId': 'main',
  'cwd': '/project',
  'title': 'Running agent',
  'provider': 'mock',
  'status': 'running',
};

JsonMap _tool(int seq) => {
  'seq': seq,
  'seqStart': seq,
  'seqEnd': seq,
  'turnId': 'turn-1',
  'timestamp': '2026-05-01T09:00:00.000Z',
  'item': {
    'type': 'tool_call',
    'callId': 'call-$seq',
    'name': 'shell',
    'status': 'completed',
    'detail': {'type': 'shell', 'command': 'echo step $seq', 'output': 'step $seq'},
  },
};

JsonMap _text(int seq, String type, String text) => {
  'seq': seq,
  'seqStart': seq,
  'seqEnd': seq,
  'turnId': 'turn-1',
  'timestamp': '2026-05-01T09:00:00.000Z',
  'item': {'type': type, 'text': text},
};

/// A running turn: a prompt, a long answer, then a run of tool rows.
List<JsonMap> _history() => [
  _text(1, 'user_message', 'Run the checks'),
  for (var i = 2; i <= 6; i++)
    _text(i, 'assistant_message', 'Paragraph $i. ${'Long reasoning text. ' * 12}'),
  for (var i = 7; i <= 30; i++) _tool(i),
];

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
  testWidgets('a detached reader is not shaken by rows arriving below it', (tester) async {
    SharedPreferences.setMockInitialValues({});
    debugDefaultTargetPlatformOverride = TargetPlatform.linux;
    tester.view.physicalSize = const Size(424, 924);
    tester.view.devicePixelRatio = 1;
    addTearDown(tester.view.reset);
    late PaseoLocalizations strings;
    await tester.runAsync(() async {
      strings = await PaseoLocalizations.load(const Locale('en'));
    });
    final visible = ValueNotifier(true);
    addTearDown(visible.dispose);
    final entries = _history();
    final pendingPermissions = <JsonMap>[];
    var promptRequests = 0;

    late _TestDaemon wire;
    wire = _TestDaemon((request) {
      switch (request['type']) {
        case 'agent.timeline.set_subscription.request':
          return {'subscriptionId': 'timeline-subscription'};
        case 'agent.provider_subagents.list.request':
          return {'subagents': []};
        case 'agent.timeline.list_prompts.request':
          promptRequests++;
          return {
            'epoch': 'epoch-1',
            'prompts': [
              for (final row in entries)
                if (jsonMap(row['item'])['type'] == 'user_message')
                  {'seq': row['seqStart'], 'preview': jsonMap(row['item'])['text']},
            ],
          };
        case 'fetch_agent_timeline_request':
          return {
            'agentId': _agentId,
            'agent': {..._runningAgent, 'pendingPermissions': pendingPermissions},
            'direction': 'tail',
            'epoch': 'epoch-1',
            'window': {'minSeq': 1, 'maxSeq': entries.length, 'nextSeq': entries.length + 1},
            'reset': false,
            'staleCursor': false,
            'gap': false,
            'hasOlder': false,
            'hasNewer': false,
            'entries': entries,
          };
      }
      return {};
    });
    final client = PaseoClient(
      host: const HostConfig(id: 'host', label: 'host', endpoint: 'ws://127.0.0.1:1/ws'),
      clientId: 'reading-hold-regression-client',
      autoReconnect: false,
      connector: (endpoint, {headers = const {}, protocols}) async => wire,
    );
    final connecting = client.connect();
    for (var i = 0; i < 5; i++) {
      await tester.pump(const Duration(milliseconds: 10));
    }
    await connecting;
    final store = AppStore(connectHosts: false, discoverLocalHosts: false);
    store.hosts.add(HostState(config: client.host, client: client, serverId: 'host'));

    try {
      await tester.pumpWidget(
        PaseoScope(
          store: store,
          child: MaterialApp(
            locale: const Locale('en'),
            localizationsDelegates: [_Loaded<PaseoLocalizations>(strings)],
            theme: paseoTheme(brightness: Brightness.dark, preferences: const {}),
            home: Scaffold(
              body: ValueListenableBuilder<bool>(
                valueListenable: visible,
                child: const AgentPanel(target: _target),
                builder: (context, shown, child) => Offstage(
                  offstage: !shown,
                  child: TickerMode(
                    enabled: shown,
                    child: PanelActivity(
                      visible: shown,
                      focused: shown,
                      appActive: shown,
                      child: child!,
                    ),
                  ),
                ),
              ),
            ),
          ),
        ),
      );
      for (var i = 0; i < 30; i++) {
        await tester.pump(const Duration(milliseconds: 16));
      }
      final scrollable = find.byType(Scrollable).first;
      final position = tester.state<ScrollableState>(scrollable).position;
      expect(position.pixels, 0, reason: 'opens at the newest edge');

      // Scroll up into the history (a downward drag on the inverted list),
      // stopping mid-transcript, so the stream detaches.
      await tester.drag(scrollable, const Offset(0, 260));
      for (var i = 0; i < 60; i++) {
        await tester.pump(const Duration(milliseconds: 16));
      }
      final detached = position.pixels;
      expect(detached, greaterThan(100));
      // What the reader is looking at: the end of the answer above the tools.
      final reading = find.textContaining('Paragraph 6.', findRichText: true);
      final readingTop = tester.getTopLeft(reading).dy;

      // Tool rows keep arriving below the reader, as in a running turn, then
      // stop; through both the row on screen must not move a pixel.
      final tops = <double>{};
      final rest = <double>{};
      var seq = 31;
      for (var frame = 0; frame < 150; frame++) {
        if (frame < 120 && frame % 10 == 0) {
          entries.add(_tool(seq));
          wire.emit('agent_stream', {
            'agentId': _agentId,
            'seq': seq,
            'epoch': 'epoch-1',
            'timestamp': '2026-05-01T09:00:00.000Z',
            'event': {'type': 'timeline', 'item': jsonMap(_tool(seq)['item'])},
          });
          seq++;
        }
        await tester.pump(const Duration(milliseconds: 16));
        tops.add(tester.getTopLeft(reading).dy);
        if (frame >= 120) rest.add(position.pixels);
      }
      expect(tops, {readingTop}, reason: 'the row being read shakes or drifts');
      expect(rest, hasLength(1), reason: 'the list never comes to rest');
      expect(position.pixels, greaterThan(detached), reason: 'the new rows grew the list below');

      // The hold never fights the reader's own scrolling.
      final held = position.pixels;
      await tester.drag(scrollable, const Offset(0, 120));
      for (var i = 0; i < 60; i++) {
        await tester.pump(const Duration(milliseconds: 16));
      }
      expect(position.pixels, greaterThan(held + 80), reason: 'the hold undid the scroll');

      final editor = tester.widget<EditableText>(find.byType(EditableText).first).controller;
      const editing = TextEditingValue(
        text: 'Keep this unfinished draft',
        selection: TextSelection(baseOffset: 5, extentOffset: 9),
      );
      editor.value = editing;
      await tester.pump();
      final drafts = ChatDrafts.instance;
      final draftKey = ChatDrafts.key('host', 'main', _agentId);
      for (final minimized in [false, true]) {
        final anchorTop = tester.getTopLeft(reading).dy;
        final hiddenOffset = position.pixels;
        final requestsBefore = promptRequests;
        if (minimized) {
          tester.binding.handleAppLifecycleStateChanged(AppLifecycleState.inactive);
          tester.binding.handleAppLifecycleStateChanged(AppLifecycleState.hidden);
        } else {
          visible.value = false;
        }
        await tester.pump();
        final messageId = 'background-prompt-$minimized';
        drafts.update(draftKey, {
          'pending': {
            'messageId': messageId,
            'text': 'A submitted prompt',
            'attachments': [],
            'images': [],
          },
        });
        final prompt = _text(seq, 'user_message', 'A submitted prompt');
        jsonMap(prompt['item'])['clientMessageId'] = messageId;
        entries.add(prompt);
        wire.emit('agent_stream', {
          'agentId': _agentId,
          'seq': seq++,
          'epoch': 'epoch-1',
          'timestamp': '2026-05-01T09:00:00.000Z',
          'event': {'type': 'timeline', 'item': prompt['item']},
        });
        final permission = {
          'id': 'permission-$minimized',
          'kind': 'tool',
          'name': 'Bash',
        };
        pendingPermissions.add(permission);
        wire.emit('agent_permission_request', {
          'agentId': _agentId,
          'request': permission,
        });
        for (var i = 0; i < 5; i++) {
          entries.add(_tool(seq));
          wire.emit('agent_stream', {
            'agentId': _agentId,
            'seq': seq,
            'epoch': 'epoch-1',
            'timestamp': '2026-05-01T09:00:00.000Z',
            'event': {'type': 'timeline', 'item': jsonMap(_tool(seq)['item'])},
          });
          seq++;
          await tester.pump(const Duration(milliseconds: 16));
        }
        expect(drafts.draft(draftKey)['pending'], isNull,
            reason: 'hidden delivery must still acknowledge the submitted prompt');
        expect(editor.value, editing);
        expect(position.pixels, hiddenOffset,
            reason: 'hidden streams must not adjust the retained reading position');
        expect(promptRequests, requestsBefore,
            reason: 'hidden streams must not refresh the presentation prompt index');

        if (minimized) {
          tester.binding.handleAppLifecycleStateChanged(AppLifecycleState.inactive);
          tester.binding.handleAppLifecycleStateChanged(AppLifecycleState.resumed);
        } else {
          visible.value = true;
        }
        for (var i = 0; i < 30; i++) {
          await tester.pump(const Duration(milliseconds: 16));
        }
        expect(tester.getTopLeft(reading).dy, anchorTop,
            reason: 'revealing accumulated rows must preserve the reading anchor');
        expect(editor.value, editing);
        expect(promptRequests, requestsBefore + 1,
            reason: 'the outline catches up once when the stream is shown');
      }

      // All background permissions and rows become reachable on return.
      position.jumpTo(0);
      for (var i = 0; i < 30; i++) {
        await tester.pump(const Duration(milliseconds: 16));
      }
      expect(find.byType(PermissionCard), findsNWidgets(2));
      expect(position.pixels, 0);
      visible.value = false;
      await tester.pump();
      entries.add(_tool(seq));
      wire.emit('agent_stream', {
        'agentId': _agentId,
        'seq': seq,
        'epoch': 'epoch-1',
        'timestamp': '2026-05-01T09:00:00.000Z',
        'event': {'type': 'timeline', 'item': jsonMap(_tool(seq)['item'])},
      });
      await tester.pump(const Duration(milliseconds: 16));
      visible.value = true;
      for (var i = 0; i < 30; i++) {
        await tester.pump(const Duration(milliseconds: 16));
      }
      expect(position.pixels, 0, reason: 'a sticky reader still follows the newest edge on return');
      expect(editor.value, editing);
    } finally {
      if (tester.binding.lifecycleState == AppLifecycleState.hidden) {
        tester.binding.handleAppLifecycleStateChanged(AppLifecycleState.inactive);
      }
      tester.binding.handleAppLifecycleStateChanged(AppLifecycleState.resumed);
      await tester.pumpWidget(const SizedBox.shrink());
      await store.disposeAsync();
      client.dispose();
      await tester.runAsync(() => wire.close());
      debugDefaultTargetPlatformOverride = null;
    }
  });
}

/// The daemon endpoint replaced with controlled wire responses; production
/// request correlation and validation stay in place.
class _TestDaemon implements DaemonTransport {
  _TestDaemon(this.respond);
  final FutureOr<JsonMap> Function(JsonMap) respond;
  final _messages = StreamController<Object>();
  @override
  Stream<Object> get messages => _messages.stream;

  void emit(String type, JsonMap payload) {
    if (!_messages.isClosed) {
      _messages.add(jsonEncode({
        'type': 'session',
        'message': {'type': type, 'payload': payload},
      }));
    }
  }

  @override
  void send(Object data) {
    final envelope = jsonMap(jsonDecode(data as String));
    if (envelope['type'] == 'hello') {
      emit('status', {
        'status': 'server_info',
        'serverId': 'host',
        'features': {
          'ownedSubscriptions': true,
          'workspaceMultiplicity': true,
          'providerSubagents': true,
          'agentTimelinePromptIndex': true,
        },
      });
      return;
    }
    if (envelope['type'] == 'ping') {
      _messages.add(jsonEncode({'type': 'pong'}));
      return;
    }
    final request = jsonMap(envelope['message']);
    final type = request['type'] as String;
    final responseType = type.endsWith('request')
        ? '${type.substring(0, type.length - 'request'.length)}response'
        : '${type}_response';
    Future<JsonMap>.sync(() => respond(request)).then((payload) {
      emit(responseType, {...payload, 'requestId': request['requestId']});
    });
  }

  @override
  Future<void> close([int? code, String? reason]) async {
    await _messages.close();
  }
}
