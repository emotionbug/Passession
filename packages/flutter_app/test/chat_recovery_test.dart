import 'dart:async';
import 'dart:convert';

import 'package:flutter_test/flutter_test.dart';
import 'package:shared_preferences/shared_preferences.dart';

import 'package:paseo_sessions/core/client.dart';
import 'package:paseo_sessions/core/host_config.dart';
import 'package:paseo_sessions/core/json.dart';
import 'package:paseo_sessions/features/chat/chat_controller.dart';
import 'package:paseo_sessions/features/chat/chat_delivery.dart';
import 'package:paseo_sessions/features/chat/chat_drafts.dart';
import 'package:paseo_sessions/features/chat/creation_recovery.dart';

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();
  final drafts = ChatDrafts.instance;

  setUpAll(() async {
    SharedPreferences.setMockInitialValues({});
    await drafts.initialize();
  });

  test('a persisted draft sends its text, images and wire attachments', () async {
    final key = ChatDrafts.key('server', 'workspace', 'agent');
    final uploaded = {
      'type': 'uploaded_file', 'id': 'upload', 'fileName': 'notes.txt',
      'mimeType': 'text/plain', 'size': 12, 'path': '/uploads/notes.txt',
    };
    final issue = {
      'type': 'forge_issue', 'mimeType': 'application/paseo-forge-issue', 'forge': 'github',
      'number': 7, 'title': 'Issue', 'url': 'https://example.test/issues/7', 'body': 'Context',
    };
    const image = {'data': 'iVBORw0KGgo=', 'mimeType': 'image/png'};
    drafts.update(key, {
      'text': '  Keep this unsent prompt  ',
      'attachments': [
        uploaded,
        {'kind': 'workspace_file', 'path': 'src/main.dart', 'selection': {'kind': 'line_range', 'startLine': 2, 'endLine': 4}},
        issue,
      ],
      'images': [image],
    });
    await drafts.flush();
    final saved = jsonMap(jsonDecode((await SharedPreferences.getInstance()).getString('paseo.chat.drafts.v1')!));
    expect(saved.object('drafts').object(key)['text'], '  Keep this unsent prompt  ');
    final message = agentMessageFields('agent', drafts.snapshot(key));
    expect(message['text'], 'Keep this unsent prompt');
    final attachments = jsonList(message['attachments']).map(jsonMap).toList();
    expect(attachments.first, uploaded);
    expect(attachments[1]['text'], contains('Lines: 2-4'));
    expect(attachments[2], issue);
    expect(jsonList(message['images']), [image]);
  });

  test('an ambiguous message receipt cannot release its retry identity', () async {
    final wire = _TestDaemon((request) => {
      'agentId': 'agent',
      'accepted': false,
      'error': 'agent_request_outcome_unknown',
    });
    final client = await _connect(wire);
    try {
      await expectLater(
        client.request(
          'send_agent_message_request',
          agentMessageFields('agent', _message('ambiguous-message')),
          responseType: 'send_agent_message_response',
        ),
        throwsA(isA<DaemonException>()
            .having((error) => error.outcomeUnknown, 'unknown outcome', true)
            .having(definitelyRejected, 'safe to replace retry identity', false)),
      );
    } finally {
      await client.close();
      client.dispose();
    }
  });

  test(
    'failed creation RPC retains the actual agent and prompt error',
    () async {
      final receipt = _failedCreation(unknown: false);
      final wire = _TestDaemon(
        (request) => {
          'error': receipt['error'],
          'agent': receipt['agent'],
          'creation': receipt,
        },
      );
      final client = await _connect(wire);
      try {
        final result = await createWithReceipt(
          client,
          'agent',
          _creationFields,
          recover: false,
        );
        expect(jsonMap(result['agent'])['id'], 'created-agent');
        expect(result['promptError'], receipt['error']);
        expect(result['creation'], receipt);
        expect(wire.requests.map((request) => request['type']), [
          'agent.create.request',
        ]);
      } finally {
        await client.close();
        client.dispose();
      }
    },
  );

  test(
    'unknown creation failure recovers the same intent without creating again',
    () async {
      final receipt = _failedCreation(unknown: true);
      final wire = _TestDaemon((request) {
        if (request['type'] == 'creation.subscribe.request') {
          expect(request['idempotencyKey'], _creationFields['idempotencyKey']);
          expect(request['subscribe'], false);
          return {'error': null, 'snapshot': receipt};
        }
        return {
          'error': receipt['error'],
          'agent': receipt['agent'],
          'creation': receipt,
        };
      });
      final client = await _connect(wire);
      final unknownWithAgent = isA<DaemonException>()
          .having(
            (error) => definitelyRejected(error),
            'safe to clear identity',
            false,
          )
          .having((error) => error.outcomeUnknown, 'unknown outcome', true)
          .having(
            (error) => jsonMap(jsonMap(error.payload)['creation'])['agentId'],
            'actual agent identity',
            'created-agent',
          );
      try {
        await expectLater(
          createWithReceipt(client, 'agent', _creationFields, recover: false),
          throwsA(unknownWithAgent),
        );
        await expectLater(
          createWithReceipt(client, 'agent', _creationFields, recover: true),
          throwsA(unknownWithAgent),
        );
        expect(wire.requests.map((request) => request['type']), [
          'agent.create.request',
          'creation.subscribe.request',
        ]);
      } finally {
        await client.close();
        client.dispose();
      }
    },
  );

  test(
    'a recovered prompt failure opens its existing agent without replay',
    () async {
      final receipt = _failedCreation(unknown: false);
      final wire = _TestDaemon(
        (request) => {'error': null, 'snapshot': receipt},
      );
      final client = await _connect(wire);
      try {
        final result = await createWithReceipt(
          client,
          'agent',
          _creationFields,
          recover: true,
        );
        expect(jsonMap(result['agent'])['id'], 'created-agent');
        expect(result['promptError'], receipt['error']);
        expect(wire.requests.single['type'], 'creation.subscribe.request');
      } finally {
        await client.close();
        client.dispose();
      }
    },
  );

  test(
    'a creation interrupted by a lost connection resubmits after reconnecting only without a receipt',
    () async {
      final first = _TestDaemon((request) => Completer<JsonMap>().future);
      final second = _TestDaemon((request) {
        switch (request['type']) {
          case 'creation.subscribe.request':
            expect(request['idempotencyKey'], _creationFields['idempotencyKey']);
            return {'subscriptionId': 'creation', 'snapshot': null, 'error': null};
          case 'agent.create.request':
            return {
              'agent': _completedCreation['agent'],
              'error': null,
              'creation': _completedCreation,
            };
        }
        return {};
      });
      final client = await _connectEach([first, second]);
      try {
        final creation = createWithReceipt(
          client,
          'agent',
          _creationFields,
          recover: false,
        );
        await pumpEventQueue();
        expect(first.requests.single['type'], 'agent.create.request');
        await first.close();
        await pumpEventQueue();
        await client.connect();
        final result = await creation;
        expect(jsonMap(result['agent'])['id'], 'created-agent');
        expect(second.requests.map((request) => request['type']).take(2), [
          'creation.subscribe.request',
          'agent.create.request',
        ]);
      } finally {
        await client.close();
        client.dispose();
      }
    },
  );

  test(
    'a creation interrupted by a lost connection follows its receipt without replaying it',
    () async {
      final first = _TestDaemon((request) => Completer<JsonMap>().future);
      late final _TestDaemon second;
      second = _TestDaemon((request) {
        if (request['type'] == 'creation.subscribe.request') {
          Timer.run(
            () => second._emit('agent.create.update', {
              ..._completedCreation,
              'subscriptionId': 'creation',
            }),
          );
          return {
            'subscriptionId': 'creation',
            'snapshot': {..._completedCreation, 'revision': 2, 'phase': 'agent_ready'},
            'error': null,
          };
        }
        return {};
      });
      final client = await _connectEach([first, second]);
      try {
        final creation = createWithReceipt(
          client,
          'agent',
          _creationFields,
          recover: false,
        );
        await pumpEventQueue();
        await first.close();
        await pumpEventQueue();
        await client.connect();
        final result = await creation;
        expect(jsonMap(result['agent'])['id'], 'created-agent');
        expect(
          second.requests.map((request) => request['type']),
          isNot(contains('agent.create.request')),
        );
      } finally {
        await client.close();
        client.dispose();
      }
    },
  );

  test('closing the client ends a pending creation as an unknown outcome', () async {
    final wire = _TestDaemon((request) => Completer<JsonMap>().future);
    final client = await _connect(wire);
    final creation = createWithReceipt(
      client,
      'agent',
      _creationFields,
      recover: false,
    );
    await pumpEventQueue();
    await client.close();
    await expectLater(
      creation,
      throwsA(
        isA<DaemonException>().having(
          definitelyRejected,
          'safe to replace creation identity',
          false,
        ),
      ),
    );
    client.dispose();
  });

  test(
    'recovering a definitely failed creation resubmits the same intent',
    () async {
      final wire = _TestDaemon((request) {
        if (request['type'] == 'creation.subscribe.request') {
          return {
            'error': null,
            'snapshot': {
              ..._failedCreation(unknown: false),
              'failedStage': 'agent',
              'agent': null,
              'error': 'Provider failed to start',
            },
          };
        }
        expect(request['idempotencyKey'], _creationFields['idempotencyKey']);
        return {
          'agent': _completedCreation['agent'],
          'error': null,
          'creation': _completedCreation,
        };
      });
      final client = await _connect(wire);
      try {
        final result = await createWithReceipt(
          client,
          'agent',
          _creationFields,
          recover: true,
        );
        expect(jsonMap(result['agent'])['id'], 'created-agent');
        expect(wire.requests.map((request) => request['type']), [
          'creation.subscribe.request',
          'agent.create.request',
        ]);
      } finally {
        await client.close();
        client.dispose();
      }
    },
  );

  test('a create handoff is held per host and agent until released', () {
    final handoffs = CreateHandoffs.instance;
    handoffs.markSent('host-a', 'created-agent');
    expect(handoffs.active('host-a', 'created-agent'), isTrue);
    expect(handoffs.active('host-b', 'created-agent'), isFalse);
    expect(handoffs.active('host-a', null), isFalse);
    handoffs.clearByAgent('host-b', 'created-agent');
    expect(handoffs.active('host-a', 'created-agent'), isTrue);
    handoffs.clearByAgent('host-a', 'created-agent');
    expect(handoffs.active('host-a', 'created-agent'), isFalse);
  });

  test('late queued edit survives an automatic delivery acknowledgement', () async {
    const serverId = 'queue-edit-race';
    final key = ChatDrafts.key(serverId, 'workspace', 'agent');
    final original = _message('queue-message');
    drafts.update(key, {
      'text': 'Separate draft',
      'queue': [original],
    });
    await drafts.flush();
    final sending = Completer<JsonMap>();
    final acknowledge = Completer<JsonMap>();
    final wire = _TestDaemon((request) {
      switch (request['type']) {
        case 'agent.timeline.set_subscription.request':
          return {
            'subscriptionId': 'timeline',
            'agentIds': ['agent'],
          };
        case 'fetch_agent_request':
          // Another client can start a turn after this snapshot. The eventual
          // send must steer, not use the daemon's default interrupt behavior.
          return {
            'agent': {
              'id': 'agent',
              'status': 'idle',
              'activeTurn': null,
              'pendingPermissions': [],
              'archivedAt': null,
            },
          };
        case 'send_agent_message_request':
          sending.complete(request);
          return acknowledge.future;
        case 'subscription.release.request':
          return {'error': null};
        default:
          throw StateError('Unexpected request: ${request['type']}');
      }
    });
    final client = await _connect(wire, serverId: serverId);
    final release = ChatDelivery.attach(client, serverId);
    try {
      final request = await sending.future.timeout(const Duration(seconds: 5));
      expect(request['activeTurnBehavior'], 'steer');
      expect(
        jsonMap(jsonList(drafts.draft(key)['queue']).single)['state'],
        'sending',
      );
      expect(
        drafts.editQueuedMessage(key, original, 'Edited while sending'),
        false,
      );
      expect(
        jsonMap(jsonList(drafts.draft(key)['queue']).single)['text'],
        original['text'],
      );
      final emptied = Completer<void>();
      void watchQueue() {
        if (jsonList(drafts.draft(key)['queue']).isEmpty &&
            !emptied.isCompleted) {
          emptied.complete();
        }
      }

      drafts.addListener(watchQueue);
      try {
        acknowledge.complete({'accepted': true, 'error': null});
        await emptied.future.timeout(const Duration(seconds: 5));
      } finally {
        drafts.removeListener(watchQueue);
      }
      expect(
        drafts.draft(key)['text'],
        'Separate draft\n\nEdited while sending',
      );
      expect(drafts.draft(key)['attachments'], original['attachments']);
      await drafts.flush();
      // Let the owned subscription release acknowledgement finish before close.
      await Future<void>.delayed(Duration.zero);
    } finally {
      if (!acknowledge.isCompleted) {
        acknowledge.complete({'accepted': true, 'error': null});
      }
      await client.close();
      release();
      client.dispose();
    }
  });

  test(
    'editing after the queue entry has completed saves a reviewable draft',
    () async {
      final key = ChatDrafts.key('completed-edit', 'workspace', 'agent');
      final original = _message('already-delivered');
      drafts.update(key, {'queue': [], 'text': ''});
      expect(drafts.editQueuedMessage(key, original, 'Keep this edit'), false);
      expect(drafts.draft(key)['text'], 'Keep this edit');
      expect(drafts.draft(key)['queue'], isEmpty);
      await drafts.flush();
    },
  );

  test(
    'editing an untouched queue entry keeps its position and identity',
    () async {
      final key = ChatDrafts.key('safe-edit', 'workspace', 'agent');
      final first = _message('first');
      final second = _message('second');
      drafts.update(key, {
        'queue': [first, second],
        'text': '',
      });
      expect(
        drafts.editQueuedMessage(key, first, 'Edited before delivery'),
        true,
      );
      final queue = jsonList(drafts.draft(key)['queue']).map(jsonMap).toList();
      expect(queue.map((item) => item['messageId']), ['first', 'second']);
      expect(queue.first['text'], 'Edited before delivery');
      expect(drafts.draft(key)['text'], '');
      await drafts.flush();
    },
  );

  test(
    'pending message persistence preserves its entire retry fingerprint',
    () async {
      final key = ChatDrafts.key('pending-replay', 'workspace', 'agent');
      final message = {
        ..._message('pending'),
        'activeTurnBehavior': 'interrupt',
      };
      final originalFields = agentMessageFields('agent', message);
      drafts.update(key, {'pending': message});
      await drafts.flush();
      final prefs = await SharedPreferences.getInstance();
      final saved = jsonMap(
        jsonDecode(prefs.getString('paseo.chat.drafts.v1')!),
      );
      final restored = jsonMap(
        jsonMap(jsonMap(saved['drafts'])[key])['pending'],
      );
      expect(agentMessageFields('agent', restored), originalFields);
      expect(
        agentMessageFields('agent', restored)['activeTurnBehavior'],
        'interrupt',
      );
    },
  );

  test(
    'send-now retry of automatic delivery preserves steer and message identity',
    () {
      final queued = _message('automatic');
      final attempted = queuedMessageForDelivery(queued, activeTurnBehavior: 'steer');
      final persisted = jsonMap(
        jsonDecode(jsonEncode({...attempted, 'state': 'unacknowledged'})),
      );
      final retried = queuedMessageForDelivery(persisted, activeTurnBehavior: 'interrupt');
      expect(
        agentMessageFields('agent', retried),
        agentMessageFields('agent', attempted),
      );
      expect(retried['messageId'], queued['messageId']);
      expect(retried['activeTurnBehavior'], 'steer');
    },
  );

  test('legacy unknown queue retry preserves omitted behavior instead of adding interrupt', () {
    final pending = {..._message('legacy'), 'state': 'unacknowledged'};
    final retried = queuedMessageForDelivery(pending, activeTurnBehavior: 'interrupt');
    expect(
      agentMessageFields('agent', retried).containsKey('activeTurnBehavior'),
      false,
    );
    expect(retried['messageId'], 'legacy');
  });

  test('a first send-now attempt uses the given send preference', () {
    final queued = _message('fresh');
    expect(
      queuedMessageForDelivery(queued, activeTurnBehavior: 'steer')['activeTurnBehavior'],
      'steer',
    );
    expect(
      queuedMessageForDelivery(queued, activeTurnBehavior: 'interrupt')['activeTurnBehavior'],
      'interrupt',
    );
  });
}

const JsonMap _creationFields = {
  'idempotencyKey': 'creation-intent',
  'workspaceId': 'workspace',
  'config': {'provider': 'codex'},
  'initialPrompt': 'Original prompt',
  'clientMessageId': 'initial-message',
  'images': [],
  'attachments': [],
  'subscribe': false,
};

JsonMap _failedCreation({required bool unknown}) => {
  'kind': 'agent',
  'idempotencyKey': 'creation-intent',
  'revision': 3,
  'phase': 'failed',
  'failedStage': 'prompt',
  'outcomeUnknown': unknown,
  'error': 'Initial prompt failed',
  'workspaceId': 'workspace',
  'agentId': 'created-agent',
  'agent': {
    'id': 'created-agent',
    'workspaceId': 'workspace',
    'title': 'Created agent',
  },
};

const JsonMap _completedCreation = {
  'kind': 'agent',
  'idempotencyKey': 'creation-intent',
  'revision': 4,
  'phase': 'completed',
  'error': null,
  'workspaceId': 'workspace',
  'agentId': 'created-agent',
  'agent': {
    'id': 'created-agent',
    'workspaceId': 'workspace',
    'title': 'Created agent',
  },
};

JsonMap _message(String id) => {
  'messageId': id,
  'text': 'Original queued text',
  'state': 'queued',
  'images': [],
  'attachments': [
    {
      'type': 'text',
      'mimeType': 'text/plain',
      'title': 'Context',
      'text': 'Kept attachment',
    },
  ],
};

Future<PaseoClient> _connect(
  _TestDaemon wire, {
  String serverId = 'test-server',
}) async {
  final client = PaseoClient(
    host: HostConfig(
      id: serverId,
      label: serverId,
      endpoint: 'ws://127.0.0.1:1/ws',
    ),
    clientId: 'chat-regression-client',
    autoReconnect: false,
    connector: (endpoint, {headers = const {}, protocols}) async => wire,
  );
  await client.connect();
  return client;
}

/// One host whose every (re)connection opens the next wire.
Future<PaseoClient> _connectEach(List<_TestDaemon> wires) async {
  final remaining = List.of(wires);
  final client = PaseoClient(
    host: HostConfig(
      id: 'test-server',
      label: 'test-server',
      endpoint: 'ws://127.0.0.1:1/ws',
    ),
    clientId: 'chat-regression-client',
    autoReconnect: false,
    connector: (endpoint, {headers = const {}, protocols}) async =>
        remaining.removeAt(0),
  );
  await client.connect();
  return client;
}

/// Exercises production request correlation, validation, and exception creation;
/// only the remote daemon endpoint is replaced with controlled wire responses.
class _TestDaemon implements DaemonTransport {
  _TestDaemon(this.respond);
  final FutureOr<JsonMap> Function(JsonMap) respond;
  final requests = <JsonMap>[];
  final _messages = StreamController<Object>();
  @override
  Stream<Object> get messages => _messages.stream;

  void _emit(String type, JsonMap payload) {
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
      _emit('status', {
        'status': 'server_info',
        'serverId': 'test-daemon',
        'features': {'ownedSubscriptions': true, 'workspaceMultiplicity': true},
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
    final responseType =
        '${type.substring(0, type.length - 'request'.length)}response';
    Future<JsonMap>.sync(() => respond(request)).then(
      (payload) {
        _emit(responseType, {...payload, 'requestId': request['requestId']});
      },
      onError: (Object error, StackTrace stack) {
        if (!_messages.isClosed) _messages.addError(error, stack);
      },
    );
  }

  @override
  Future<void> close([int? code, String? reason]) async {
    await _messages.close();
  }
}
