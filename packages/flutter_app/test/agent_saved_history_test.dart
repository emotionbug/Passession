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
import 'package:paseo_sessions/features/chat/agent_panel.dart';
import 'package:paseo_sessions/features/chat/chat_controller.dart';
import 'package:paseo_sessions/features/chat/chrome/stream_states.dart';
import 'package:paseo_sessions/features/chat/composer/send_button.dart';
import 'package:paseo_sessions/features/chat/permission_card.dart';
import 'package:paseo_sessions/ui/controls.dart';
import 'package:paseo_sessions/ui/localization.dart';
import 'package:paseo_sessions/ui/panel_activity.dart';
import 'package:paseo_sessions/ui/theme.dart';

/// Hosts advertising `saved_history_reads` answer the first page from the
/// saved checkpoint while the provider prepares (`historySource: "saved"`,
/// `agent.preparation`). Reading and execution readiness are separate.
const _agentId = 'a5a5a5a5-0000-4000-8000-000000000040';
const _agentTarget = PanelTarget(
  kind: 'agent',
  serverId: 'host',
  workspaceId: 'main',
  cwd: '/project',
  id: _agentId,
);

PanelTarget _childTarget(String id) => PanelTarget(
  kind: 'provider_subagent',
  serverId: 'host',
  workspaceId: 'main',
  cwd: '/project',
  id: id,
  data: const {'parentAgentId': _agentId},
);

const _preparing = {'status': 'preparing'};

JsonMap _agent({
  JsonMap? preparation,
  String status = 'idle',
  List<JsonMap> permissions = const [],
  JsonMap? activeTurn,
  String? availability,
}) => {
  'id': _agentId,
  'provider': 'codex',
  'cwd': '/project',
  'workspaceId': 'main',
  'title': 'Saved agent',
  'status': status,
  'pendingPermissions': permissions,
  'activeTurn': ?activeTurn,
  'preparation': ?preparation,
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
      : {
          'type': 'assistant_message',
          'text': 'Saved answer $seq. ${'Saved context. ' * 6}',
        },
};

List<JsonMap> _rows(int from, int to) => [
  for (var seq = from; seq <= to; seq++) _row(seq),
];

JsonMap _page(
  String direction,
  List<JsonMap> entries, {
  String epoch = 'epoch-saved',
  bool hasOlder = false,
  bool reset = false,
  String source = 'saved',
  JsonMap? agent,
}) {
  final start = entries.isEmpty ? null : entries.first['seqStart'] as int;
  final end = entries.isEmpty ? null : entries.last['seqEnd'] as int;
  return {
    'agentId': _agentId,
    'agent': ?agent,
    'direction': direction,
    'projection': 'projected',
    'epoch': epoch,
    'window': {'minSeq': 1, 'maxSeq': end ?? 100, 'nextSeq': (end ?? 100) + 1},
    'reset': reset,
    'staleCursor': false,
    'gap': false,
    'hasOlder': hasOlder,
    'hasNewer': false,
    'startCursor': start == null ? null : {'epoch': epoch, 'seq': start},
    'endCursor': end == null ? null : {'epoch': epoch, 'seq': end},
    'entries': entries,
    'historySource': source,
  };
}

JsonMap _childPage(
  String childId,
  List<JsonMap> rows, {
  required String epoch,
  bool hasOlder = false,
  JsonMap? nativeRefresh,
}) => {
  'parentAgentId': _agentId,
  'subagentId': childId,
  'provider': 'codex',
  'epoch': epoch,
  'reset': false,
  'staleCursor': false,
  'gap': false,
  'window': {
    'minSeq': 1,
    'maxSeq': rows.isEmpty ? 0 : rows.last['seqEnd'],
    'nextSeq': rows.isEmpty ? 1 : (rows.last['seqEnd'] as int) + 1,
  },
  'hasOlder': hasOlder,
  'hasNewer': false,
  'rows': rows,
  'nativeRefresh': ?nativeRefresh,
};

String _text(JsonMap entry) => stringValue(jsonMap(entry['item'])['text']);

typedef _Responder = FutureOr<JsonMap> Function(JsonMap request);

_Wire _daemon(_Responder timeline) => _Wire((request) {
  switch (request['type']) {
    case 'agent.timeline.set_subscription.request':
      return {'subscriptionId': 'timeline-subscription'};
    case 'agent.provider_subagents.list.request':
      return {
        'subagents': [
          {
            'id': 'child-a',
            'parentAgentId': _agentId,
            'title': 'Explore',
            'status': 'completed',
          },
          {
            'id': 'child-b',
            'parentAgentId': _agentId,
            'title': 'Review',
            'status': 'running',
          },
        ],
      };
    case 'send_agent_message_request':
      return {'agentId': _agentId, 'accepted': true};
    case 'agent.timeline.list_prompts.request':
      return {'epoch': 'epoch-saved', 'prompts': <JsonMap>[]};
    case 'fetch_agent_timeline_request':
    case 'agent.provider_subagents.timeline.get.request':
      return timeline(request);
  }
  return {};
});

/// Each connect (the first and every reconnect) takes the next wire.
Future<PaseoClient> _connectEach(List<_Wire> wires) async {
  final remaining = List.of(wires);
  final client = PaseoClient(
    host: const HostConfig(
      id: 'host',
      label: 'host',
      endpoint: 'ws://127.0.0.1:1/ws',
    ),
    clientId: 'saved-history-regression-client',
    autoReconnect: false,
    connector: (endpoint, {headers = const {}, protocols}) async =>
        remaining.removeAt(0),
  );
  await client.connect();
  return client;
}

Future<(_Wire, PaseoClient)> _connect(_Responder timeline) async {
  final wire = _daemon(timeline);
  return (wire, await _connectEach([wire]));
}

Future<void> _settle() async {
  for (var i = 0; i < 30; i++) {
    await Future<void>.delayed(Duration.zero);
  }
}

/// The composer's editor: the only writable EditableText (transcript rows
/// render read-only selectable text).
Finder _composerEditor() => find.byWidgetPredicate(
  (widget) => widget is EditableText && !widget.readOnly,
);

class _Harness {
  _Harness(this.wire, this.client, this.controller);
  final _Wire wire;
  final PaseoClient client;
  final ChatController controller;

  List<JsonMap> fetches([String type = 'fetch_agent_timeline_request']) =>
      wire.requests.where((r) => r['type'] == type).toList();

  Future<void> close() async {
    controller.dispose();
    await client.close();
    client.dispose();
  }
}

Future<_Harness> _open(
  _Responder timeline, {
  PanelTarget target = _agentTarget,
}) async {
  final (wire, client) = await _connect(timeline);
  final controller = ChatController(client, target);
  await controller.start();
  await _settle();
  return _Harness(wire, client, controller);
}

void main() {
  test('saved history paints the bounded tail while the provider prepares; older pages stay readable', () async {
    final h = await _open((request) {
      if (request['direction'] == 'before') {
        return _page('before', _rows(21, 60), hasOlder: true);
      }
      return _page(
        'tail',
        _rows(61, 100),
        hasOlder: true,
        // A stale saved snapshot: its run state and permission are not live.
        agent: _agent(
          preparation: _preparing,
          status: 'running',
          activeTurn: {
            'turnId': 'turn-old',
            'startedAt': '2026-05-01T08:00:00.000Z',
          },
          permissions: [
            {'id': 'stale-permission', 'kind': 'tool', 'name': 'Bash'},
          ],
        ),
      );
    });
    try {
      final first = h.fetches().single;
      expect(first['direction'], 'tail');
      expect(first['limit'], timelineFetchPageSize);
      expect(h.controller.timeline, hasLength(40));
      expect(_text(h.controller.timeline.last), 'Question 100.');
      expect(h.controller.historySaved, isTrue);
      expect(h.controller.syncStatus, TimelineSyncStatus.ready);
      expect(h.controller.execution, AgentExecutionStatus.preparing);
      expect(
        h.controller.running,
        isFalse,
        reason: 'a preparing provider runs no turn',
      );
      expect(
        h.controller.permissions,
        isEmpty,
        reason: 'a saved permission is not answerable',
      );
      expect(h.controller.turns, isEmpty);

      await h.controller.older();
      final older = h.fetches().last;
      expect(older['direction'], 'before');
      expect(jsonMap(older['cursor'])['seq'], 61);
      expect(h.controller.timeline, hasLength(80));
      expect(h.controller.startCursor?['seq'], 21);
      expect(h.controller.execution, AgentExecutionStatus.preparing);
      expect(h.fetches().map((r) => r['direction']), [
        'tail',
        'before',
      ], reason: 'reading saved history waits for no provider refresh');
    } finally {
      await h.close();
    }
  });

  test('provider readiness replaces saved history through one after page, then live rows apply', () async {
    final h = await _open((request) {
      if (request['direction'] == 'after') {
        // Provider replay moved the epoch: the host resets to its tail.
        return _page(
          'after',
          _rows(62, 101),
          epoch: 'epoch-live',
          hasOlder: true,
          reset: true,
          source: 'live',
          agent: _agent(),
        );
      }
      return _page(
        'tail',
        _rows(61, 100),
        hasOlder: true,
        agent: _agent(preparation: _preparing),
      );
    });
    try {
      expect(h.controller.execution, AgentExecutionStatus.preparing);
      h.wire.emit('agent_update', {'kind': 'upsert', 'agent': _agent()});
      await _settle();
      final refresh = h.fetches().last;
      expect(refresh['direction'], 'after');
      expect(refresh['cursor'], {'epoch': 'epoch-saved', 'seq': 100});
      expect(h.controller.epoch, 'epoch-live');
      expect(h.controller.historySaved, isFalse);
      expect(h.controller.execution, AgentExecutionStatus.ready);
      expect(h.controller.timeline, hasLength(40));
      expect(_text(h.controller.timeline.first), 'Question 62.');
      expect(
        _text(h.controller.timeline.last),
        startsWith('Saved answer 101.'),
      );

      // A repeated ready snapshot does not refresh again.
      h.wire.emit('agent_update', {'kind': 'upsert', 'agent': _agent()});
      await _settle();
      expect(h.fetches(), hasLength(2));

      h.wire.emit('agent_stream', {
        'agentId': _agentId,
        'seq': 102,
        'epoch': 'epoch-live',
        'timestamp': '2026-05-01T09:00:00.000Z',
        'event': {
          'type': 'timeline',
          'item': {'type': 'user_message', 'text': 'Live question.'},
        },
      });
      await _settle();
      expect(_text(h.controller.timeline.last), 'Live question.');
      expect(h.controller.endCursor?['seq'], 102);
      expect(
        h.fetches(),
        hasLength(2),
        reason: 'contiguous live delivery needs no fetch',
      );
    } finally {
      await h.close();
    }
  });

  test(
    'a failed preparation keeps saved rows and its retry restarts preparation',
    () async {
      var phase = 'failed';
      Completer<JsonMap>? held;
      final h = await _open((request) {
        if (request['direction'] == 'tail') {
          return _page(
            'tail',
            _rows(61, 100),
            hasOlder: true,
            agent: _agent(
              preparation: {
                'status': 'failed',
                'error': 'codex app-server exited (1)',
              },
            ),
          );
        }
        if (phase == 'store-error') return {'error': 'Agent store unavailable'};
        held = Completer<JsonMap>();
        return held!.future;
      });
      try {
        expect(h.controller.execution, AgentExecutionStatus.failed);
        expect(h.controller.preparationError, 'codex app-server exited (1)');
        expect(h.controller.timeline, hasLength(40));
        expect(h.controller.syncStatus, TimelineSyncStatus.ready);
        expect(
          h.controller.error,
          isNull,
          reason: 'a provider failure is not a history failure',
        );

        h.controller.retryPreparation();
        expect(h.controller.preparationRetrying, isTrue);
        await _settle();
        expect(h.fetches().last['direction'], 'after');
        expect(
          h.controller.preparationRetrying,
          isTrue,
          reason: 'retrying until the fetch settles',
        );
        h.controller.retryPreparation();
        await _settle();
        expect(
          h.fetches(),
          hasLength(2),
          reason: 'an in-flight retry is not repeated',
        );

        held!.complete(
          _page('after', const [], agent: _agent(preparation: _preparing)),
        );
        await _settle();
        expect(h.controller.preparationRetrying, isFalse);
        expect(h.controller.execution, AgentExecutionStatus.preparing);
        expect(h.controller.timeline, hasLength(40));

        // The provider fails again; a retry whose fetch fails keeps every row.
        h.wire.emit('agent_update', {
          'kind': 'upsert',
          'agent': _agent(
            preparation: {'status': 'failed', 'error': 'still failing'},
          ),
        });
        await _settle();
        phase = 'store-error';
        h.controller.retryPreparation();
        await _settle();
        expect(h.controller.preparationRetrying, isFalse);
        expect(h.controller.syncStatus, TimelineSyncStatus.error);
        expect(h.controller.execution, AgentExecutionStatus.failed);
        expect(h.controller.preparationError, 'still failing');
        expect(h.controller.timeline, hasLength(40));
        expect(_text(h.controller.timeline.last), 'Question 100.');
      } finally {
        await h.close();
      }
    },
  );

  test(
    'a last-persisted observation neither grants nor revokes live permissions',
    () async {
      final h = await _open(
        (request) =>
            _page('tail', _rows(1, 2), source: 'live', agent: _agent()),
      );
      try {
        h.wire.emit('agent_permission_request', {
          'agentId': _agentId,
          'request': {'id': 'live-permission', 'kind': 'tool', 'name': 'Bash'},
        });
        await _settle();
        expect(h.controller.permissions.keys, ['live-permission']);

        h.wire.emit('agent_update', {
          'kind': 'upsert',
          'agent': _agent(
            availability: 'last_persisted',
            permissions: [
              {'id': 'persisted-permission', 'kind': 'tool', 'name': 'Edit'},
            ],
          ),
        });
        await _settle();
        expect(h.controller.permissions.keys, ['live-permission']);
        expect(h.controller.execution, AgentExecutionStatus.ready);

        h.wire.emit('agent_update', {
          'kind': 'upsert',
          'agent': _agent(preparation: _preparing),
        });
        await _settle();
        expect(
          h.controller.permissions,
          isEmpty,
          reason: 'a re-preparing provider has no pending request',
        );
        expect(h.controller.running, isFalse);
      } finally {
        await h.close();
      }
    },
  );

  test('a fresh open authorizes execution only from a live snapshot', () async {
    final tail = Completer<JsonMap>();
    final (wire, client) = await _connect((request) => tail.future);
    final controller = ChatController(client, _agentTarget);
    try {
      unawaited(controller.start());
      await _settle();
      expect(controller.agent, isEmpty);
      expect(
        controller.executionAuthorized,
        isFalse,
        reason: 'nothing has been observed yet',
      );

      tail.complete(
        _page(
          'tail',
          _rows(61, 100),
          hasOlder: true,
          source: 'live',
          agent: _agent(
            availability: 'last_persisted',
            permissions: [
              {'id': 'persisted-permission', 'kind': 'tool', 'name': 'Bash'},
            ],
          ),
        ),
      );
      await _settle();
      expect(controller.timeline, hasLength(40));
      expect(controller.execution, AgentExecutionStatus.ready);
      expect(
        controller.executionAuthorized,
        isFalse,
        reason: 'a saved observation alone never authorizes a send',
      );
      expect(controller.permissions, isEmpty);

      wire.emit('agent_update', {'kind': 'upsert', 'agent': _agent()});
      await _settle();
      expect(controller.executionAuthorized, isTrue);

      wire.emit('agent_permission_request', {
        'agentId': _agentId,
        'request': {'id': 'live-permission', 'kind': 'tool', 'name': 'Bash'},
      });
      wire.emit('agent_update', {
        'kind': 'upsert',
        'agent': _agent(availability: 'last_persisted'),
      });
      await _settle();
      expect(
        controller.executionAuthorized,
        isTrue,
        reason: 'a busy observation does not revoke live authority',
      );
      expect(controller.permissions.keys, ['live-permission']);

      wire.emit('agent_update', {
        'kind': 'upsert',
        'agent': _agent(preparation: _preparing),
      });
      await _settle();
      expect(controller.executionAuthorized, isFalse);
      wire.emit('agent_update', {
        'kind': 'upsert',
        'agent': _agent(availability: 'last_persisted'),
      });
      await _settle();
      expect(
        controller.executionAuthorized,
        isFalse,
        reason: 'after preparation only a live snapshot restores authority',
      );
    } finally {
      controller.dispose();
      await client.close();
      client.dispose();
    }
  });

  test('a reconnect drops execution authority until a fresh live snapshot', () async {
    final first = _daemon(
      (request) => _page(
        'tail',
        _rows(61, 100),
        hasOlder: true,
        source: 'live',
        agent: _agent(),
      ),
    );
    // The new connection first observes only a busy (last-persisted) snapshot.
    final second = _daemon(
      (request) => _page(
        stringValue(request['direction']),
        const [],
        source: 'live',
        agent: _agent(availability: 'last_persisted'),
      ),
    );
    final client = await _connectEach([first, second]);
    final controller = ChatController(client, _agentTarget);
    try {
      await controller.start();
      await _settle();
      expect(controller.executionAuthorized, isTrue);
      first.emit('agent_permission_request', {
        'agentId': _agentId,
        'request': {'id': 'live-permission', 'kind': 'tool', 'name': 'Bash'},
      });
      await _settle();

      await first.close();
      await _settle();
      expect(controller.connected, isFalse);
      expect(controller.executionAuthorized, isFalse);

      await client.connect();
      await _settle();
      expect(controller.connected, isTrue);
      expect(
        second.requests.where(
          (r) => r['type'] == 'fetch_agent_timeline_request',
        ),
        isNotEmpty,
        reason: 'the reconnect catches up on the new connection',
      );
      expect(controller.agent['snapshotAvailability'], 'last_persisted');
      expect(
        controller.executionAuthorized,
        isFalse,
        reason: 'authority from the previous connection does not carry over',
      );
      expect(
        controller.timeline,
        hasLength(40),
        reason: 'history stays readable',
      );
      expect(controller.permissions.keys, [
        'live-permission',
      ], reason: 'a busy observation does not revoke a live permission');

      second.emit('agent_update', {'kind': 'upsert', 'agent': _agent()});
      await _settle();
      expect(controller.executionAuthorized, isTrue);
    } finally {
      controller.dispose();
      await client.close();
      client.dispose();
    }
  });

  test(
    'a parent lists child metadata without fetching any child transcript',
    () async {
      final h = await _open(
        (request) =>
            _page('tail', _rows(1, 4), source: 'live', agent: _agent()),
      );
      try {
        expect(h.controller.subagents.map((s) => s['id']), [
          'child-a',
          'child-b',
        ]);
        h.wire.emit('agent.provider_subagents.update', {
          'kind': 'timeline',
          'parentAgentId': _agentId,
          'subagentId': 'child-b',
          'provider': 'codex',
          'item': {'type': 'assistant_message', 'text': 'Child body'},
          'timestamp': '2026-05-01T09:00:00.000Z',
          'seq': 9,
          'epoch': 'child-epoch',
        });
        await _settle();
        expect(
          h.fetches('agent.provider_subagents.timeline.get.request'),
          isEmpty,
        );
        expect(
          h.fetches('agent.provider_subagents.list.request'),
          hasLength(1),
        );
        expect(h.controller.timeline.map(_text), isNot(contains('Child body')));
      } finally {
        await h.close();
      }
    },
  );

  test('a child refresh failure stays visible over its stored rows and retry reads again', () async {
    var retried = false;
    final h = await _open((request) {
      if (request['direction'] == 'tail') {
        return _childPage(
          'child-a',
          _rows(11, 20),
          epoch: 'child-1',
          hasOlder: true,
          nativeRefresh: {'status': 'refreshing'},
        );
      }
      retried = true;
      return _childPage(
        'child-a',
        const [],
        epoch: 'child-1',
        nativeRefresh: {'status': 'refreshing'},
      );
    }, target: _childTarget('child-a'));
    void upsert(JsonMap? nativeRefresh) =>
        h.wire.emit('agent.provider_subagents.update', {
          'kind': 'upsert',
          'subagent': {
            'id': 'child-a',
            'parentAgentId': _agentId,
            'title': 'Explore',
            'status': 'completed',
            'nativeRefresh': ?nativeRefresh,
          },
        });
    try {
      expect(h.controller.childRefreshing, isTrue);
      expect(h.controller.timeline, hasLength(10));

      upsert({'status': 'failed', 'error': 'thread/read failed: 503'});
      await _settle();
      expect(h.controller.childRefreshError, 'thread/read failed: 503');
      expect(
        h.controller.timeline,
        hasLength(10),
        reason: 'a failed refresh keeps the stored rows',
      );
      expect(h.controller.syncStatus, TimelineSyncStatus.ready);

      h.controller.retryChildRefresh();
      expect(h.controller.preparationRetrying, isTrue);
      await _settle();
      expect(retried, isTrue);
      final reads = h.fetches('agent.provider_subagents.timeline.get.request');
      expect(reads.last['subagentId'], 'child-a');
      expect(reads.last['direction'], 'after');
      expect(h.controller.preparationRetrying, isFalse);
      expect(h.controller.childRefreshing, isTrue);
      expect(h.controller.childRefreshError, isNull);

      upsert(null);
      await _settle();
      expect(h.controller.childRefreshing, isFalse);
      expect(h.controller.childRefreshError, isNull);
      expect(h.controller.timeline, hasLength(10));
      expect(
        h.fetches(),
        isEmpty,
        reason: 'a child pane never reads the parent transcript',
      );
    } finally {
      await h.close();
    }
  });

  test('an open child fetches only its own bounded transcript and reinstalls a new child epoch once', () async {
    var childEpoch = 'child-1';
    Completer<JsonMap>? held;
    final h = await _open((request) {
      expect(request['type'], 'agent.provider_subagents.timeline.get.request');
      if (childEpoch == 'child-2') {
        held = Completer<JsonMap>();
        return held!.future;
      }
      return _childPage(
        'child-a',
        _rows(11, 20),
        epoch: childEpoch,
        hasOlder: true,
      );
    }, target: _childTarget('child-a'));
    void childEvent(String childId, int seq, String epoch, String text) =>
        h.wire.emit('agent.provider_subagents.update', {
          'kind': 'timeline',
          'parentAgentId': _agentId,
          'subagentId': childId,
          'provider': 'codex',
          'item': {'type': 'user_message', 'text': text},
          'timestamp': '2026-05-01T09:00:00.000Z',
          'seq': seq,
          'epoch': epoch,
        });
    try {
      final first = h
          .fetches('agent.provider_subagents.timeline.get.request')
          .single;
      expect(first['subagentId'], 'child-a');
      expect(first['parentAgentId'], _agentId);
      expect(first['direction'], 'tail');
      expect(first['limit'], timelineFetchPageSize);
      expect(
        h.fetches(),
        isEmpty,
        reason: 'a child pane never reads the parent transcript',
      );
      expect(h.fetches('agent.provider_subagents.list.request'), isEmpty);
      expect(h.controller.timeline, hasLength(10));

      childEvent('child-b', 21, 'child-1', 'Sibling row');
      childEvent('child-a', 21, 'child-1', 'Live child row');
      await _settle();
      expect(_text(h.controller.timeline.last), 'Live child row');
      expect(h.controller.timeline.map(_text), isNot(contains('Sibling row')));
      expect(
        h.fetches('agent.provider_subagents.timeline.get.request'),
        hasLength(1),
      );

      // A background native refresh moved this child to a new epoch.
      childEpoch = 'child-2';
      childEvent('child-a', 7, 'child-2', 'Refreshed row 7');
      childEvent('child-a', 8, 'child-2', 'Refreshed row 8');
      await _settle();
      final resync = h.fetches('agent.provider_subagents.timeline.get.request');
      expect(
        resync,
        hasLength(2),
        reason: 'one tail per announced child epoch',
      );
      expect(resync.last['direction'], 'tail');
      expect(resync.last['subagentId'], 'child-a');

      held!.complete(
        _childPage('child-a', [
          for (var seq = 1; seq <= 8; seq++)
            {
              ..._row(seq),
              'item': {'type': 'user_message', 'text': 'Refreshed row $seq'},
            },
        ], epoch: 'child-2'),
      );
      await _settle();
      expect(h.controller.epoch, 'child-2');
      expect(h.controller.timeline.map(_text), [
        for (var seq = 1; seq <= 8; seq++) 'Refreshed row $seq',
      ]);
      expect(
        h.fetches('agent.provider_subagents.timeline.get.request'),
        hasLength(2),
      );
    } finally {
      await h.close();
    }
  });

  testWidgets(
    'the panel reads and scrolls saved history while sending waits for the provider',
    (tester) async {
      SharedPreferences.setMockInitialValues({});
      debugDefaultTargetPlatformOverride = TargetPlatform.linux;
      tester.view.physicalSize = const Size(424, 924);
      tester.view.devicePixelRatio = 1;
      addTearDown(tester.view.reset);
      late PaseoLocalizations strings;
      await tester.runAsync(() async {
        strings = await PaseoLocalizations.load(const Locale('en'));
      });

      var ready = false;
      final olderGate = Completer<JsonMap>();
      final staleRunning = _agent(
        preparation: _preparing,
        status: 'running',
        activeTurn: {
          'turnId': 'turn-old',
          'startedAt': '2026-05-01T08:00:00.000Z',
        },
        permissions: [
          {'id': 'stale-permission', 'kind': 'tool', 'name': 'Bash'},
        ],
      );
      late _Wire wire;
      late PaseoClient client;
      final connecting = _connect((request) {
        switch (request['direction']) {
          case 'tail':
            return _page(
              'tail',
              _rows(61, 100),
              hasOlder: true,
              agent: staleRunning,
            );
          case 'before':
            return olderGate.future;
        }
        return ready
            ? _page('after', const [], source: 'live', agent: _agent())
            : _page('after', const [], agent: _agent(preparation: _preparing));
      }).then((connected) => (wire, client) = connected);
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

      List<JsonMap> fetches(String direction) => wire.requests
          .where(
            (r) =>
                r['type'] == 'fetch_agent_timeline_request' &&
                r['direction'] == direction,
          )
          .toList();
      AgentPreparationCallout callout() =>
          tester.widget<AgentPreparationCallout>(
            find.byType(AgentPreparationCallout),
          );
      VoidCallback? send() => tester
          .widget<ComposerSendButton>(find.byType(ComposerSendButton))
          .onPressed;

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

        // The saved tail is on screen, not a loading overlay or a full-screen error.
        expect(find.byType(HistorySyncOverlay), findsNothing);
        expect(find.byType(AgentLoadErrorView), findsNothing);
        expect(
          find.textContaining('Question 100.', findRichText: true),
          findsWidgets,
        );
        expect(callout().failed, isFalse);
        expect(
          find.byType(PermissionCard),
          findsNothing,
          reason: 'saved permissions are not answerable',
        );
        expect(
          find.byType(ComposerCancelButton),
          findsNothing,
          reason: 'a saved turn is not cancellable',
        );

        final editor = tester
            .widget<EditableText>(_composerEditor())
            .controller;
        const draft = TextEditingValue(
          text: 'Keep this draft until the agent is ready',
          selection: TextSelection.collapsed(offset: 10),
        );
        editor.value = draft;
        await frames(4);
        expect(send(), isNull, reason: 'sending waits for execution readiness');

        // Older saved history loads during preparation and keeps the reading anchor.
        final scrollable = find.byType(Scrollable).first;
        final position = tester.state<ScrollableState>(scrollable).position;
        // A lazily laid out list corrects its extent as it builds: keep going
        // to the history start until the older page is requested.
        for (var i = 0; i < 10 && fetches('before').isEmpty; i++) {
          position.jumpTo(position.maxScrollExtent);
          await frames(4);
        }
        await frames();
        final older = fetches('before').single;
        expect(jsonMap(older['cursor'])['seq'], 61);
        // The reading anchor is a saved row actually mounted inside the viewport.
        final viewport = tester.getRect(scrollable);
        Finder? visible;
        for (var seq = 61; seq <= 100 && visible == null; seq++) {
          final candidate = find.textContaining(
            seq.isEven ? 'Question $seq.' : 'Saved answer $seq.',
            findRichText: true,
          );
          if (candidate.evaluate().length != 1) continue;
          final rect = tester.getRect(candidate);
          if (rect.top >= viewport.top && rect.bottom <= viewport.bottom) {
            visible = candidate;
          }
        }
        expect(
          visible,
          isNotNull,
          reason: 'a saved row is visible at the history start',
        );
        final anchor = visible!;
        final anchorTop = tester.getTopLeft(anchor).dy;
        final readingOffset = position.pixels;
        olderGate.complete(_page('before', _rows(21, 60)));
        await frames();
        expect(
          tester.getTopLeft(anchor).dy,
          anchorTop,
          reason: 'the older page moved the reader',
        );
        expect(
          position.pixels,
          readingOffset,
          reason: 'prepending older rows moved the scroll offset',
        );

        // Failure stays inline with the provider's error; history and draft stay.
        wire.emit('agent_update', {
          'kind': 'upsert',
          'agent': _agent(
            preparation: {
              'status': 'failed',
              'error': 'codex app-server exited (1)',
            },
          ),
        });
        await frames();
        expect(callout().failed, isTrue);
        expect(
          find.textContaining('codex app-server exited (1)'),
          findsWidgets,
        );
        expect(
          anchor,
          findsOneWidget,
          reason: 'saved rows stay on screen after the failure',
        );
        expect(editor.value, draft);
        expect(send(), isNull);

        await tester.tap(
          find.descendant(
            of: find.byType(AgentPreparationCallout),
            matching: find.byType(PaseoButton),
          ),
        );
        await frames();
        expect(
          fetches('after'),
          hasLength(1),
          reason: 'retry restarts preparation through a fetch',
        );
        expect(callout().failed, isFalse);

        ready = true;
        wire.emit('agent_update', {'kind': 'upsert', 'agent': _agent()});
        await frames();
        expect(
          fetches('after'),
          hasLength(2),
          reason: 'readiness refreshes the saved history once',
        );
        expect(find.byType(AgentPreparationCallout), findsNothing);
        expect(send(), isNotNull);
        expect(editor.value, draft);
        expect(anchor, findsOneWidget);
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
  final _Responder respond;
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
          'projectedSubagentTimeline': true,
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
