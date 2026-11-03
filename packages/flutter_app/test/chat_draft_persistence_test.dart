import 'dart:async';
import 'dart:convert';

import 'package:flutter_test/flutter_test.dart';
import 'package:shared_preferences/shared_preferences.dart';
import 'package:shared_preferences_platform_interface/shared_preferences_platform_interface.dart';

import 'package:paseo_sessions/core/json.dart';
import 'package:paseo_sessions/features/chat/chat_drafts.dart';

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();
  final drafts = ChatDrafts.instance;
  late _Preferences store;

  setUpAll(() async {
    SharedPreferences.resetStatic();
    store = _Preferences();
    SharedPreferencesStorePlatform.instance = store;
    await drafts.initialize();
  });

  test('flush preserves the latest edit and another session image without one write per pending keystroke', () async {
    final first = ChatDrafts.key('host', 'workspace', 'editing');
    final other = ChatDrafts.key('host', 'workspace', 'image-draft');
    final image = {
      'data': base64Encode(List.filled(256 * 1024, 17)),
      'mimeType': 'image/png',
    };
    drafts.update(other, {
      'images': [image],
      'text': 'Keep this other draft',
    });
    await drafts.flush();
    store.writes.clear();
    final gate = Completer<void>();
    final started = Completer<void>();
    store.blockNext = gate;
    store.nextStarted = started;
    drafts.update(first, {'text': 'first edit'});
    await started.future;
    for (var i = 0; i < 50; i++) {
      drafts.update(first, {'text': 'latest edit $i'});
    }
    var flushed = false;
    final flushing = drafts.flush().then((_) => flushed = true);
    await Future<void>.delayed(Duration.zero);
    expect(
      flushed,
      isFalse,
      reason: 'the flush boundary waits for the in-flight write',
    );
    gate.complete();
    await flushing;
    final persisted = store.savedDrafts;
    expect(persisted.object(first)['text'], 'latest edit 49');
    expect(persisted.object(other)['text'], 'Keep this other draft');
    expect(persisted.object(other)['images'], [image]);
    expect(
      store.writes.length,
      lessThanOrEqualTo(2),
      reason:
          'queued edits must coalesce behind an in-flight storage operation',
    );
  });

  test('failed persistence remains visible and a later successful save retains pending message identity', () async {
    final key = ChatDrafts.key('host', 'workspace', 'failed-save');
    final message = {
      'messageId': 'durable-message',
      'text': 'Send once',
      'images': [
        {'data': 'AQID', 'mimeType': 'image/png'},
      ],
    };
    store.failNext = true;
    drafts.update(key, {'pending': message});
    await expectLater(drafts.flush(), throwsStateError);
    expect(drafts.draft(key)['pending'], message);
    drafts.update(key, {'text': 'A newer unsent edit'});
    await drafts.flush();
    expect(drafts.persistenceError, isNull);
    expect(store.savedDrafts.object(key)['pending'], message);
    expect(store.savedDrafts.object(key)['text'], 'A newer unsent edit');
  });

  test('a flush boundary completes while a later unrelated edit is still being written', () async {
    final key = ChatDrafts.key('host', 'workspace', 'send-boundary');
    final other = ChatDrafts.key('host', 'workspace', 'later-typing');
    final gates = List.generate(3, (_) => Completer<void>());
    final starts = List.generate(3, (_) => Completer<void>());
    store.blockNext = gates[0];
    store.nextStarted = starts[0];
    try {
      drafts.update(key, {'text': 'first'});
      await starts[0].future.timeout(const Duration(seconds: 5));
      final pending = {
        'messageId': 'send-boundary-message',
        'text': 'Send after durable save',
      };
      drafts.update(key, {'pending': pending});
      final boundary = drafts.flush();
      store.blockNext = gates[1];
      store.nextStarted = starts[1];
      gates[0].complete();
      await starts[1].future.timeout(const Duration(seconds: 5));
      store.blockNext = gates[2];
      store.nextStarted = starts[2];
      drafts.update(other, {'text': 'Typing after the send boundary'});
      gates[1].complete();
      await starts[2].future.timeout(const Duration(seconds: 5));
      await boundary.timeout(const Duration(seconds: 5));
      expect(store.savedDrafts.object(key)['pending'], pending);
      expect(
        store.savedDrafts.object(other),
        isEmpty,
        reason: 'the later write is still held; it must not delay an already durable send',
      );
    } finally {
      for (final gate in gates) {
        if (!gate.isCompleted) gate.complete();
      }
      await drafts.flush();
    }
    expect(
      store.savedDrafts.object(other)['text'],
      'Typing after the send boundary',
    );
  });
}

class _Preferences extends InMemorySharedPreferencesStore {
  _Preferences() : super.empty();
  final writes = <String>[];
  String? _saved;
  Completer<void>? blockNext;
  Completer<void>? nextStarted;
  bool failNext = false;
  JsonMap get savedDrafts => jsonMap(jsonDecode(_saved!)).object('drafts');

  @override
  Future<bool> setValue(String valueType, String key, Object value) async {
    if (key == 'flutter.paseo.chat.drafts.v1') {
      writes.add(value as String);
      final gate = blockNext;
      blockNext = null;
      nextStarted?.complete();
      nextStarted = null;
      if (gate != null) await gate.future;
      if (failNext) {
        failNext = false;
        return false;
      }
    }
    final saved = await super.setValue(valueType, key, value);
    if (saved && key == 'flutter.paseo.chat.drafts.v1') {
      _saved = value as String;
    }
    return saved;
  }
}
