import 'dart:async';

import 'package:flutter_test/flutter_test.dart';
import 'package:paseo_sessions/core/app_store.dart';

/// Subscription cancellation crosses both the test clock and completed futures
/// created outside it. Drain both queues instead of waiting on a frozen clock.
Future<T> settleTestFuture<T>(WidgetTester tester, Future<T> future) async {
  var settled = false;
  unawaited(
    future.then<void>(
      (_) {
        settled = true;
      },
      onError: (Object error, StackTrace stack) {
        settled = true;
      },
    ),
  );
  for (var turn = 0; !settled && turn < 20; turn++) {
    await tester.runAsync(() => Future<void>.delayed(Duration.zero));
    await tester.pump();
  }
  expect(settled, isTrue, reason: 'Fixture operation did not settle');
  return await future;
}

Future<void> disposeTestStore(WidgetTester tester, AppStore store) =>
    settleTestFuture(tester, store.disposeAsync());
