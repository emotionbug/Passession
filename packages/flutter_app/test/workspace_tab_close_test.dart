import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:shared_preferences/shared_preferences.dart';
import 'package:paseo_sessions/ui/localization.dart';

import 'fixtures/tab_close_fixture.dart';

Finder closeButton(String title) => find.descendant(
  of: find.ancestor(of: find.text(title), matching: find.byType(InkWell)).first,
  matching: find.byTooltip('Close'),
);

Future<void> frame(WidgetTester tester) async {
  await tester.runAsync(() => PaseoLocalizations.load(const Locale('en')));
  await tester.pump();
  await tester.pump(const Duration(milliseconds: 100));
}

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();
  setUpAll(() => PaseoLocalizations.load(const Locale('en')));
  setUp(() => SharedPreferences.setMockInitialValues({}));

  testWidgets(
    'tab X preserves sessions and honors a denied child close guard',
    (tester) async {
      tester.view.physicalSize = const Size(1440, 1000);
      tester.view.devicePixelRatio = 1;
      addTearDown(tester.view.resetPhysicalSize);
      addTearDown(tester.view.resetDevicePixelRatio);
      final fixture = TabCloseFixture();
      try {
        await fixture.initialize();
        await tester.pumpWidget(fixture.app());
        await frame(tester);
        fixture.requests.clear();
        final before = jsonEncode(fixture.store.hosts.single.agents);
        final target = fixture.target('root-one');
        await tester.tap(closeButton('Running conversation'));
        await frame(tester);
        expect(fixture.store.targetFor(target.key), isNull);
        expect(jsonEncode(fixture.store.hosts.single.agents), before);
        expect(
          fixture.requests.where(
            (r) => [
              'archive_agent_request',
              'delete_agent_request',
              'kill_agent_request',
              'cancel_agent_request',
              'update_agent_request',
            ].contains(r['type']),
          ),
          isEmpty,
        );
        fixture.store.open(target);
        fixture.store.focusedPane.selectedKey = null;
        await frame(tester);
        expect(closeButton('Running conversation'), findsOneWidget);
        expect(
          fixture.store.hosts.single.agents.first['persistence']['sessionId'],
          'native-root-one',
        );

        fixture.requests.clear();
        final child = fixture.target('child-one');
        final remove = fixture.store.registerCloseGuard(
          child.key,
          () async => false,
        );
        await tester.tap(closeButton('Pinned child'));
        await frame(tester);
        expect(fixture.store.targetFor(child.key)?.id, 'child-one');
        expect(
          fixture.requests.where((r) => r['type'] == 'update_agent_request'),
          isEmpty,
        );
        remove();
        await tester.tap(closeButton('Pinned child'));
        await frame(tester);
        expect(fixture.store.targetFor(child.key), isNull);
        expect(
          fixture.store.hosts.single.agents.last['persistence']['sessionId'],
          'native-child',
        );
        expect(
          fixture.requests.where(
            (r) => [
              'archive_agent_request',
              'delete_agent_request',
              'kill_agent_request',
              'cancel_agent_request',
            ].contains(r['type']),
          ),
          isEmpty,
        );
        await tester.pumpWidget(const SizedBox());
      } finally {
        await tester.pumpWidget(const SizedBox());
        final closing = fixture.store.disposeAsync();
        await frame(tester);
        await closing;
      }
    },
  );
}
