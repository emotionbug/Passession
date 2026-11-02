import 'dart:async';
import 'dart:convert';

import 'package:flutter/foundation.dart';
import 'package:shared_preferences/shared_preferences.dart';
import 'package:uuid/uuid.dart';

import '../../core/json.dart';

/// Durable client-owned drafts and queues. A queue is never a daemon API.
class ChatDrafts extends ChangeNotifier {
  ChatDrafts._();
  static final instance = ChatDrafts._();
  final Map<String, JsonMap> _drafts = {};
  final Map<String, List<JsonMap>> _inbox = {};
  final Set<String> _intents = {};
  final Set<String> _dirtyDrafts = {};
  Future<void>? _initialization;
  Future<void> _writes = Future.value();
  Object? persistenceError;

  static String key(String serverId, String workspaceId, String id) =>
      jsonEncode([serverId, workspaceId, id]);
  static String workspaceKey(String serverId, String workspaceId) =>
      jsonEncode([serverId, workspaceId]);
  Iterable<String> keysForServer(String serverId) => _drafts.keys.where((key) {
    final identity = jsonList(jsonDecode(key));
    return identity.isNotEmpty && identity.first == serverId;
  });

  Future<void> initialize() => _initialization ??= _load();
  Future<void> _load() async {
    try {
      final prefs = await SharedPreferences.getInstance();
      final encoded = prefs.getString('paseo.chat.drafts.v1');
      if (encoded != null) {
        final saved = jsonMap(jsonDecode(encoded));
        for (final entry in jsonMap(saved['drafts']).entries) {
          if (_dirtyDrafts.contains(entry.key)) continue;
          final value = jsonMap(entry.value);
          value['queue'] = jsonList(value['queue']).map((item) {
            final queued = jsonMap(item);
            return queued['state'] == 'sending'
                ? {
                    ...queued,
                    'state': 'unacknowledged',
                    'error': 'Delivery is unconfirmed. Check the exact message status before retrying.',
                  }
                : queued;
          }).toList();
          _drafts[entry.key] = value;
        }
        for (final entry in jsonMap(saved['inbox']).entries) {
          final current = _inbox.putIfAbsent(entry.key, () => []);
          final known = current.map(jsonEncode).toSet();
          current.addAll(
            jsonList(entry.value)
                .map(jsonMap)
                .where((item) => known.add(jsonEncode(item))),
          );
        }
        _intents.addAll(jsonList(saved['intents']).whereType<String>());
      }
    } catch (error) {
      persistenceError = error;
    }
    notifyListeners();
  }

  JsonMap draft(String key) => _drafts.putIfAbsent(
    key,
    () => {
      'text': '',
      'attachments': <JsonMap>[],
      'images': <JsonMap>[],
      'queue': <JsonMap>[],
      'config': <String, dynamic>{},
    },
  );

  void update(String key, JsonMap fields) {
    _dirtyDrafts.add(key);
    final current = draft(key);
    if (['text', 'attachments', 'images'].any(fields.containsKey)) {
      current['editorGeneration'] = (current['editorGeneration'] as int? ?? 0) + 1;
    }
    current.addAll(fields);
    _save();
  }

  void addAttachments(
    String serverId,
    String workspaceId,
    List<JsonMap> attachments, {
    String? intentId,
  }) {
    if (intentId != null && !_intents.add(intentId)) return;
    final inbox = _inbox.putIfAbsent(
      workspaceKey(serverId, workspaceId),
      () => [],
    );
    for (final attachment in attachments) {
      if (!inbox.any((item) => jsonEncode(item) == jsonEncode(attachment))) {
        inbox.add(attachment);
      }
    }
    _save();
  }

  void addDraftAttachments(
    String key,
    List<JsonMap> attachments, {
    String? intentId,
  }) {
    if (intentId != null && !_intents.add(intentId)) return;
    final current = jsonList(draft(key)['attachments']).map(jsonMap).toList();
    final known = current.map(jsonEncode).toSet();
    current.addAll(
      attachments.where((attachment) => known.add(jsonEncode(attachment))),
    );
    update(key, {'attachments': current});
  }

  void takeAttachments(String serverId, String workspaceId, String draftKey) {
    final pending = _inbox.remove(workspaceKey(serverId, workspaceId));
    if (pending == null || pending.isEmpty) return;
    final current = jsonList(draft(draftKey)['attachments'])
        .map(jsonMap)
        .toList();
    current.addAll(
      pending.where((a) => !current.any((b) => jsonEncode(a) == jsonEncode(b))),
    );
    update(draftKey, {'attachments': current});
  }

  bool ownsPending(String key, JsonMap submitted) =>
      jsonMap(draft(key)['pending'])['messageId'] == submitted['messageId'] &&
      !isDismissed(key, stringValue(submitted['messageId']));

  bool isDismissed(String key, String id) =>
      jsonList(draft(key)['dismissedMessageIds']).contains(id);

  void acknowledge(String key, JsonMap submitted) {
    final current = draft(key);
    final id = stringValue(submitted['messageId']);
    if (isDismissed(key, id)) return;
    final pending = jsonMap(current['pending']);
    if (pending.isNotEmpty && pending['messageId'] != id) return;
    // Equality of content is not ownership: identical retyping is a new edit.
    if (submitted['editorGeneration'] != null &&
        current['editorGeneration'] == submitted['editorGeneration']) {
      current['text'] = '';
      current['attachments'] = <JsonMap>[];
      current['images'] = <JsonMap>[];
      current['editorGeneration'] = (current['editorGeneration'] as int? ?? 0) + 1;
    }
    if (pending['messageId'] == id) current.remove('pending');
    if (current['sendErrorMessageId'] == id) {
      current.remove('sendError');
      current.remove('sendErrorMessageId');
    }
    _save();
  }

  /// A local tombstone also guards legacy hosts, but never claims remote cancel.
  void dismiss(String key, String id) {
    final current = draft(key);
    current['dismissedMessageIds'] = {
      ...jsonList(current['dismissedMessageIds']).whereType<String>(), id,
    }.toList();
    if (jsonMap(current['pending'])['messageId'] == id) {
      current.remove('pending');
    }
    current['queue'] = jsonList(current['queue']).map(jsonMap).toList()
      ..removeWhere((item) => item['messageId'] == id);
    if (current['sendErrorMessageId'] == id) {
      current.remove('sendError');
      current.remove('sendErrorMessageId');
    }
    _save();
  }

  /// Monotonic exact-ID delivery evidence. A canonical user row is not a receipt.
  bool applyReceipt(String key, JsonMap receipt) {
    final identity = jsonList(jsonDecode(key));
    final id = stringValue(receipt['messageId']);
    if (id.isEmpty || identity[2] != receipt['agentId'] ||
        receipt['revision'] is! int) return false;
    final current = draft(key);
    final receipts = {...jsonMap(current['receipts'])};
    final previous = jsonMap(receipts[id]);
    if ((previous['revision'] as int? ?? -1) >= (receipt['revision'] as int)) return false;
    final sticky = isDismissed(key, id) || previous['dismissed'] == true;
    receipts[id] = {...receipt, if (sticky) 'dismissed': true};
    current['receipts'] = receipts;
    if (sticky || receipt['dismissed'] == true) {
      dismiss(key, id);
      return true;
    }
    if (receiptResolved(receipt)) {
      final pending = jsonMap(current['pending']);
      if (pending['messageId'] == id) acknowledge(key, pending);
      current['queue'] = jsonList(current['queue']).map(jsonMap).toList()
        ..removeWhere((item) => item['messageId'] == id);
    } else if (receipt['phase'] == 'rejected' || receipt['phase'] == 'not_delivered') {
      final pending = jsonMap(current['pending']);
      if (pending['messageId'] == id) {
        current['failedMessage'] = pending;
        current.remove('pending');
        current['sendError'] = receipt['error'];
        current['sendErrorMessageId'] = id;
      }
      final queue = jsonList(current['queue']).map(jsonMap).toList();
      for (var i = 0; i < queue.length; i++) {
        if (queue[i]['messageId'] == id) {
          queue[i] = {...queue[i], 'state': 'rejected', 'error': receipt['error']};
        }
      }
      current['queue'] = queue;
    }
    _save();
    return true;
  }

  bool failSend(String key, JsonMap submitted, Object error, {bool rejected = false}) {
    if (!ownsPending(key, submitted)) return false;
    final current = draft(key);
    final receipt = jsonMap(jsonMap(current['receipts'])[submitted['messageId']]);
    if (receiptResolved(receipt) || receipt['dismissed'] == true) return false;
    current['sendError'] = '$error';
    current['sendErrorMessageId'] = submitted['messageId'];
    if (rejected && receipt.isEmpty) {
      current['failedMessage'] = submitted;
      current.remove('pending');
    }
    // Keep failed content separate; never write over newer editor generations.
    _save();
    return true;
  }

  /// The message a submit sends: the composer text trimmed (submit.ts /
  /// input.tsx sendMessageImpl) and the attachments.
  JsonMap snapshot(String key) {
    final d = draft(key);
    return {
      'messageId': const Uuid().v4(),
      'editorGeneration': d['editorGeneration'] as int? ?? 0,
      'text': stringValue(d['text']).trim(),
      'attachments': List.of(jsonList(d['attachments'])),
      'images': List.of(jsonList(d['images'])),
    };
  }

  /// Commit only against the snapshot shown by the edit dialog. Delivery may
  /// have started (or completed) while the user was typing.
  bool editQueuedMessage(String key, JsonMap original, String text) {
    final current = draft(key);
    final queue = jsonList(current['queue']).map(jsonMap).toList();
    final index = queue.indexWhere(
      (item) => item['messageId'] == original['messageId'],
    );
    if (index >= 0 &&
        ['queued', 'blocked', 'rejected'].contains(queue[index]['state']) &&
        jsonEncode(queue[index]) == jsonEncode(original)) {
      final edited = {
        ...queue[index],
        'text': text,
        'state': 'queued',
        'error': null,
      };
      if (original['state'] == 'rejected') {
        // An explicit edit of a definitely rejected request is a new message,
        // not a retry of the immutable receipt. Unknown outcomes cannot enter
        // this branch.
        edited['messageId'] = const Uuid().v4();
        edited.remove('deliveryAttempted');
        edited.remove('activeTurnBehavior');
      }
      queue[index] = edited;
      update(key, {'queue': queue});
      return true;
    }
    // Never overwrite a sending snapshot. Preserve the user's edited content
    // in the composer for review, including attachments, without sending it.
    final fields = <String, dynamic>{
      'text': [
        stringValue(current['text']),
        text,
      ].where((part) => part.isNotEmpty).join('\n\n'),
    };
    for (final field in ['attachments', 'images']) {
      final values = List.of(jsonList(current[field]));
      final known = values.map(jsonEncode).toSet();
      values.addAll(
        jsonList(original[field])
            .where((value) => known.add(jsonEncode(value))),
      );
      fields[field] = values;
    }
    update(key, fields);
    return false;
  }

  Future<void> flush() async {
    await _writes;
    if (persistenceError != null) {
      throw StateError('Draft persistence failed: $persistenceError');
    }
  }

  void _save() {
    notifyListeners();
    _writes = _writes
        .then((_) async {
          await initialize();
          final encoded = jsonEncode({
            'drafts': _drafts,
            'inbox': _inbox,
            'intents': _intents.toList(),
          });
          final prefs = await SharedPreferences.getInstance();
          if (!await prefs.setString('paseo.chat.drafts.v1', encoded)) {
            throw StateError('Could not save conversation drafts');
          }
          if (persistenceError != null) {
            persistenceError = null;
            notifyListeners();
          }
        })
        .catchError((Object error) {
          persistenceError = error;
          notifyListeners();
        });
  }
}

bool receiptResolved(JsonMap receipt) =>
    receipt['phase'] == 'accepted' ||
    (receipt['phase'] == 'submitted' &&
        jsonMap(receipt['correlation'])['acknowledgementPending'] == false);

bool receiptCanRetry(JsonMap receipt) =>
    receipt['phase'] == 'not_delivered' &&
    receipt['canRetry'] == true && receipt['dismissed'] != true;

JsonMap wireAttachment(JsonMap attachment) {
  if (attachment['kind'] != 'workspace_file') return attachment;
  final path = stringValue(attachment['path']);
  final selection = jsonMap(attachment['selection']);
  final lines = selection['kind'] == 'line_range'
      ? '\nLines: ${selection['startLine']}-${selection['endLine']}'
      : '';
  return {
    'type': 'text',
    'mimeType': 'text/plain',
    'title': path.split('/').last,
    'text': 'Workspace file: $path$lines',
  };
}

/// The behavior is part of the daemon's durable request fingerprint. Once an
/// attempt has begun, even an omitted behavior must be replayed unchanged.
/// [activeTurnBehavior] is the behavior of a first attempt: the queue drain
/// steers (host-runtime.ts drainQueuedAgentMessage), Send now follows the
/// send preference (index.tsx handleSendQueuedNow → submitMessage).
JsonMap queuedMessageForDelivery(JsonMap message, {required String activeTurnBehavior}) {
  final attempted =
      message['deliveryAttempted'] == true ||
      ['sending', 'unacknowledged', 'rejected'].contains(message['state']);
  return {
    ...message,
    if (!attempted) 'activeTurnBehavior': activeTurnBehavior,
    'deliveryAttempted': true,
  };
}

JsonMap agentMessageFields(String agentId, JsonMap message) => {
  'agentId': agentId,
  'text': message['text'],
  'messageId': message['messageId'],
  if (message['activeTurnBehavior'] != null)
    'activeTurnBehavior': message['activeTurnBehavior'],
  'images': jsonList(message['images']),
  'attachments': jsonList(message['attachments'])
      .map(jsonMap)
      .map(wireAttachment)
      .toList(),
};
