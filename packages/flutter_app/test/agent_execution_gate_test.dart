import 'dart:async';
import 'dart:convert';

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
import 'package:paseo_sessions/features/chat/agent_controls.dart';
import 'package:paseo_sessions/features/chat/agent_panel.dart';
import 'package:paseo_sessions/features/chat/chrome/stream_states.dart';
import 'package:paseo_sessions/features/chat/composer/send_button.dart';
import 'package:paseo_sessions/ui/localization.dart';
import 'package:paseo_sessions/ui/panel_activity.dart';
import 'package:paseo_sessions/ui/theme.dart';

/// Execution authority on the real composer: only a live agent snapshot lets
/// Enter send. This file holds a single panel widget test on purpose: the
/// process-wide [ChatDrafts] keeps its initialization and write futures, and a
/// completed future notifies new listeners through a microtask in the zone
/// that created it. A second widget test in the same isolate would await a
/// previous test's fake-async zone that no pump drains, and the composer would
/// never finish initializing.
const _agentId = 'a5a5a5a5-0000-4000-8000-000000000041';
const _agentTarget = PanelTarget(
  kind: 'agent',
  serverId: 'host',
  workspaceId: 'main',
  cwd: '/project',
  id: _agentId,
);

JsonMap _agent({String? availability}) => {
  'id': _agentId,
  'provider': 'codex',
  'cwd': '/project',
  'workspaceId': 'main',
  'title': 'Saved agent',
  'status': 'idle',
  'pendingPermissions': <JsonMap>[],
  'snapshotAvailability': ?availability,
};

/// Even sequences are prompts, odd ones answers, so no two rows merge.
JsonMap _row(int seq) => {
  'seq': seq,
  'seqStart': seq,
  'seqEnd': seq,
  'turnId': 'turn-${seq ~/ 2}',
  'timestamp': '2026-05-01T09:00:00.000Z',
  'item': seq.isEven
      ? {'type': 'user_message', 'text': 'Question $seq.'}
      : {'type': 'assistant_message', 'text': 'Saved answer $seq.'},
};

JsonMap _page(
  String direction,
  List<JsonMap> entries, {
  required JsonMap agent,
}) {
  final start = entries.isEmpty ? null : entries.first['seqStart'] as int;
  final end = entries.isEmpty ? null : entries.last['seqEnd'] as int;
  return {
    'agentId': _agentId,
    'agent': agent,
    'direction': direction,
    'projection': 'projected',
    'epoch': 'epoch-saved',
    'window': {'minSeq': 1, 'maxSeq': end ?? 100, 'nextSeq': (end ?? 100) + 1},
    'reset': false,
    'staleCursor': false,
    'gap': false,
    'hasOlder': true,
    'hasNewer': false,
    'startCursor': start == null
        ? null
        : {'epoch': 'epoch-saved', 'seq': start},
    'endCursor': end == null ? null : {'epoch': 'epoch-saved', 'seq': end},
    'entries': entries,
    'historySource': 'live',
  };
}

/// The composer's editor: the only writable EditableText (transcript rows
/// render read-only selectable text).
Finder _composerEditor() => find.byWidgetPredicate(
  (widget) => widget is EditableText && !widget.readOnly,
);

void main() {
  testWidgets(
    'a persisted-only snapshot gates Enter and agent controls until a live one arrives',
    (tester) async {
      SharedPreferences.setMockInitialValues({});
      debugDefaultTargetPlatformOverride = TargetPlatform.linux;
      // Wide desktop layout: Enter submits.
      tester.view.physicalSize = const Size(1280, 900);
      tester.view.devicePixelRatio = 1;
      addTearDown(tester.view.reset);
      late PaseoLocalizations strings;
      await tester.runAsync(() async {
        strings = await PaseoLocalizations.load(const Locale('en'));
      });
      final wire = _Wire((request) {
        switch (request['type']) {
          case 'agent.timeline.set_subscription.request':
            return {'subscriptionId': 'timeline-subscription'};
          case 'agent.provider_subagents.list.request':
            return {'subagents': <JsonMap>[]};
          case 'agent.timeline.list_prompts.request':
            return {'epoch': 'epoch-saved', 'prompts': <JsonMap>[]};
          case 'send_agent_message_request':
            return {'agentId': _agentId, 'accepted': true};
          case 'fetch_agent_timeline_request':
            // The host only ever observes a busy runtime: a saved snapshot.
            return _page(
              stringValue(request['direction']),
              request['direction'] == 'tail'
                  ? [for (var seq = 61; seq <= 100; seq++) _row(seq)]
                  : const [],
              agent: _agent(availability: 'last_persisted'),
            );
        }
        return {};
      });
      final client = PaseoClient(
        host: const HostConfig(
          id: 'host',
          label: 'host',
          endpoint: 'ws://127.0.0.1:1/ws',
        ),
        clientId: 'execution-gate-regression-client',
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
      Future<void> frames([int count = 30]) async {
        for (var i = 0; i < count; i++) {
          await tester.pump(const Duration(milliseconds: 16));
        }
      }

      List<JsonMap> sends() => wire.requests
          .where((r) => r['type'] == 'send_agent_message_request')
          .toList();
      VoidCallback? send() => tester
          .widget<ComposerSendButton>(find.byType(ComposerSendButton))
          .onPressed;
      bool controlsDisabled() =>
          tester.widget<AgentControls>(find.byType(AgentControls)).disabled;
      final notReady = find.byKey(const ValueKey('composer-not-ready'));

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
              home: const Scaffold(
                body: PanelActivity(
                  visible: true,
                  focused: true,
                  appActive: true,
                  child: AgentPanel(target: _agentTarget),
                ),
              ),
            ),
          ),
        );
        await frames();
        expect(
          find.textContaining('Question 100.', findRichText: true),
          findsWidgets,
        );
        expect(
          find.byType(AgentPreparationCallout),
          findsNothing,
          reason:
              'no preparation is reported, only execution authority is missing',
        );
        expect(controlsDisabled(), isTrue);

        final field = _composerEditor();
        await tester.tap(field);
        await tester.pump();
        final input = tester.widget<EditableText>(field);
        final editor = input.controller;
        editor.text = 'Not from a saved snapshot';
        await frames(4);
        expect(send(), isNull);
        expect(notReady, findsNothing);
        expect(
          FocusManager.instance.primaryFocus,
          same(input.focusNode),
          reason: 'Enter is delivered to the composer node itself',
        );
        final handled = await tester.sendKeyEvent(LogicalKeyboardKey.enter);
        await frames();
        // The composer's key handler consumed Enter and reached the submit (the
        // not-ready notice is its only result); nothing was sent or cleared,
        // even after draft persistence settled.
        expect(
          handled,
          isTrue,
          reason: 'the composer key handler consumed Enter',
        );
        expect(notReady, findsOneWidget);
        expect(
          sends(),
          isEmpty,
          reason: 'Enter must not send on a persisted-only snapshot',
        );
        expect(editor.text, 'Not from a saved snapshot');
        expect(controlsDisabled(), isTrue);

        wire.emit('agent_update', {'kind': 'upsert', 'agent': _agent()});
        await frames();
        expect(controlsDisabled(), isFalse);
        expect(send(), isNotNull);
        expect(notReady, findsNothing);
        expect(FocusManager.instance.primaryFocus, same(input.focusNode));
        expect(
          await tester.sendKeyEvent(LogicalKeyboardKey.enter),
          isTrue,
          reason: 'the same composer handler consumes Enter once live',
        );
        await frames();
        expect(
          sends(),
          hasLength(1),
          reason: 'the same Enter sends once the agent is live',
        );
        expect(sends().single['text'], 'Not from a saved snapshot');
        expect(notReady, findsNothing);
      } finally {
        await tester.pumpWidget(const SizedBox.shrink());
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

/// The daemon endpoint replaced with controlled wire responses; production
/// request correlation and validation stay in place.
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
  Future<void> close([int? code, String? reason]) async {
    await _messages.close();
  }
}
