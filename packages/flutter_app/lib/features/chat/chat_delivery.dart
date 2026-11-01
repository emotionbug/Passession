import 'dart:async';
import 'dart:convert';

import 'package:flutter/foundation.dart';

import '../../core/client.dart';
import '../../core/json.dart';
import 'chat_controller.dart';
import 'chat_drafts.dart';

/// Owns client queues independently of panel visibility and tab lifetime.
class ChatDelivery {
  ChatDelivery._(this.client, this.serverId) {
    _subscription = client.events.listen(_event);
    client.addListener(_schedule);
    drafts.addListener(_schedule);
    drafts.initialize().then((_) => _schedule());
  }
  static final Map<PaseoClient, ChatDelivery> _owners = {};
  static VoidCallback attach(PaseoClient client, String serverId) {
    final owner = _owners.putIfAbsent(
      client,
      () => ChatDelivery._(client, serverId),
    );
    return () {
      if (identical(_owners[client], owner)) {
        _owners.remove(client);
        owner._dispose();
      }
    };
  }

  final PaseoClient client;
  final String serverId;
  final drafts = ChatDrafts.instance;
  late final StreamSubscription<JsonMap> _subscription;
  final Set<String> _busy = {};
  final Map<String, OwnedObservation> _observations = {};
  final Set<String> _queried = {};
  OwnedObservation? _receiptObservation;
  bool _subscribing = false;
  ConnectionStatus? _lastStatus;

  static Future<void> check(PaseoClient client, String key, String id, {bool dismiss = false}) async {
    final identity = jsonList(jsonDecode(key));
    final receipt = await client.messageReceipt(stringValue(identity[2]), id, dismiss: dismiss);
    if (dismiss && receipt['dismissed'] == true &&
        receipt['agentId'] == identity[2] && receipt['messageId'] == id) {
      ChatDrafts.instance.dismiss(key, id);
    }
    ChatDrafts.instance.applyReceipt(key, receipt);
    await ChatDrafts.instance.flush();
  }

  void _apply(JsonMap receipt) {
    if (receipt.isEmpty) return;
    for (final key in drafts.keysForServer(serverId).toList()) {
      final identity = jsonList(jsonDecode(key));
      if (identity[2] == receipt['agentId']) drafts.applyReceipt(key, receipt);
    }
  }

  Future<void> _subscribeReceipts() async {
    _subscribing = true;
    try {
      final observation = await client.observe(
        type: 'session.events.set_subscription.request',
        fields: {'events': ['agent.message_receipt.update']},
        responseType: 'session.events.set_subscription.response',
        onEvent: (event) {
          if (event['type'] == 'subscription.restored') {
            _queried.clear();
            _schedule();
          }
        },
      );
      if (_disposed) {
        await observation.close();
      } else {
        _receiptObservation = observation;
        _schedule();
      }
    } catch (_) {
      // Exact-ID recovery still works when event subscription is unavailable.
    } finally {
      _subscribing = false;
    }
  }

  Future<void> _recover(String key, String id) async {
    try {
      await check(client, key, id);
    } catch (_) {
      // Keep uncertainty durable; reconnect or an explicit Check retries.
    }
  }
  bool _disposed = false, _scheduled = false;

  void _event(JsonMap event) {
    final payload = jsonMap(event['payload']);
    if (client.supports('agentMessageReceipts')) {
      _apply(jsonMap(payload['receipt']));
    }
    if (event['type'] == 'agent_permission_resolved' ||
        (event['type'] == 'agent_stream' &&
            [
              'turn_completed',
              'turn_canceled',
              'turn_failed',
              'permission_resolved',
            ].contains(jsonMap(payload['event'])['type']))) {
      _schedule();
    }
  }

  void _schedule() {
    if (_lastStatus != client.status) {
      _lastStatus = client.status;
      _queried.clear();
    }
    if (_disposed ||
        _scheduled ||
        client.status != ConnectionStatus.connected) {
      return;
    }
    _scheduled = true;
    scheduleMicrotask(() {
      _scheduled = false;
      if (_disposed) return;
      for (final key in drafts.keysForServer(serverId)) {
        if (client.supports('agentMessageReceipts')) {
          if (_receiptObservation == null && !_subscribing) {
            unawaited(_subscribeReceipts());
          }
          final current = drafts.draft(key);
          final uncertain = [
            jsonMap(current['pending']),
            ...maps(current['queue']).where((item) =>
                ['sending', 'unacknowledged'].contains(item['state'])),
          ];
          for (final message in uncertain) {
            final id = stringValue(message['messageId']);
            if (id.isNotEmpty && !drafts.isDismissed(key, id) &&
                _queried.add('$key:$id')) {
              unawaited(_recover(key, id));
            }
          }
        }
        final queue = maps(drafts.draft(key)['queue']);
        if (queue.isNotEmpty &&
            jsonMap(drafts.draft(key)['pending']).isEmpty &&
            queue.first['state'] == 'queued' &&
            !_busy.contains(key)) {
          unawaited(_drain(key));
        } else if (queue.isEmpty) {
          _observations.remove(key)?.close();
        }
      }
    });
  }

  Future<void> _drain(String key) async {
    if (!_busy.add(key)) return;
    if (jsonMap(drafts.draft(key)['pending']).isNotEmpty) {
      _busy.remove(key);
      return;
    }
    JsonMap? sending;
    try {
      final identity = jsonDecode(key) as List;
      final agentId = identity[2] as String;
      if (!_observations.containsKey(key)) {
        final observation = await client.observe(
          type: 'agent.timeline.set_subscription.request',
          fields: {
            'agentIds': [agentId],
          },
          responseType: 'agent.timeline.set_subscription.response',
          onEvent: (event) {
            if (event['type'] == 'subscription.restored') _schedule();
          },
        );
        if (_disposed) {
          await observation.close();
          return;
        }
        _observations[key] = observation;
      }
      final result = requireSuccess(
        await client.request('fetch_agent_request', {
          'agentId': agentId,
        }, responseType: 'fetch_agent_response'),
      );
      if (_disposed) return;
      final agent = jsonMap(result['agent']);
      if (agent.isEmpty) {
        throw StateError('The queued session no longer exists.');
      }
      if (agent['status'] != 'idle' ||
          agent['activeTurn'] != null ||
          jsonList(agent['pendingPermissions']).isNotEmpty ||
          agent['archivedAt'] != null) {
        return;
      }
      final queue = maps(drafts.draft(key)['queue']);
      if (jsonMap(drafts.draft(key)['pending']).isNotEmpty ||
          queue.isEmpty || queue.first['state'] != 'queued') return;
      // host-runtime.ts drainQueuedAgentMessage: a drained message steers, so
      // a turn the agent starts on its own in that moment absorbs it.
      sending = queuedMessageForDelivery(queue.first, activeTurnBehavior: 'steer');
      queue[0] = {...sending, 'state': 'sending'};
      drafts.update(key, {'queue': queue});
      await drafts.flush();
      SentUserMessages.remember(agentId, sending);
      final response = requireSuccess(
        await client.request(
          'send_agent_message_request',
          agentMessageFields(agentId, sending),
          responseType: 'send_agent_message_response',
        ),
      );
      if (client.supports('agentMessageReceipts')) {
        _apply(jsonMap(response['receipt']));
        final current = maps(drafts.draft(key)['queue']);
        final index = current.indexWhere((item) => item['messageId'] == sending!['messageId']);
        if (index >= 0 && current[index]['state'] == 'sending') {
          current[index] = {...current[index], 'state': 'unacknowledged'};
          drafts.update(key, {'queue': current});
        }
      } else {
        final current = maps(drafts.draft(key)['queue'])
          ..removeWhere((message) => message['messageId'] == sending!['messageId']);
        drafts.update(key, {'queue': current});
      }
    } catch (error) {
      if (error is DaemonException) _apply(jsonMap(error.payload?['receipt']));
      final queue = maps(drafts.draft(key)['queue']);
      if (sending != null) {
        final index = queue.indexWhere(
          (message) => message['messageId'] == sending!['messageId'],
        );
        if (index >= 0) {
          queue[index] = {
            ...queue[index],
            'state': queue[index]['state'] == 'rejected' || definitelyRejected(error)
                ? 'rejected' : 'unacknowledged',
            'error': queue[index]['state'] == 'rejected'
                ? queue[index]['error'] ?? '$error' : '$error',
          };
        }
      } else if (queue.isNotEmpty) {
        // A failed readiness/observation check sent nothing; preserve queue order.
        queue[0] = {...queue.first, 'state': 'blocked', 'error': '$error'};
      }
      drafts.update(key, {'queue': queue});
    } finally {
      _busy.remove(key);
      if (sending != null) _schedule();
    }
  }

  void _dispose() {
    _disposed = true;
    drafts.removeListener(_schedule);
    client.removeListener(_schedule);
    _subscription.cancel();
    _receiptObservation?.close();
    for (final observation in _observations.values) {
      observation.close();
    }
    _observations.clear();
  }
}
