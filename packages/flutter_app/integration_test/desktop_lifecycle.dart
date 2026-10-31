import 'dart:async';
import 'dart:convert';
import 'dart:developer' show Timeline;
import 'dart:io';
import 'dart:ui' show FramePhase, FrameTiming;

import 'package:desktop_multi_window/desktop_multi_window.dart';
import 'package:flutter/widgets.dart';
import 'package:window_manager/window_manager.dart';

import 'package:paseo_sessions/main.dart' as production;
import 'package:paseo_sessions/features/chat/stream/tool_call_display.dart';
import 'package:paseo_sessions/features/chat/tool_call_details.dart';
import 'package:paseo_sessions/platform/desktop/desktop_host.dart';
import 'package:paseo_sessions/features/files/vim_engine.dart';
import 'package:paseo_sessions/ui/shell.dart';

final proof = Directory(Platform.environment['PASEO_DESKTOP_SMOKE_PROOF']!);
void require(bool value, String message) {
  if (!value) throw StateError(message);
}

Future<void> save(String name, Object value) async {
  final file = File('${proof.path}/$name');
  final temporary = File('${file.path}.$pid.tmp');
  await temporary.writeAsString(jsonEncode(value));
  await temporary.rename(file.path);
}

void record(String phase, [Object? value]) {
  final row = jsonEncode({'phase': phase, 'value': value});
  File('${proof.path}/events.jsonl')
      .writeAsStringSync('$row\n', mode: FileMode.append);
  stdout.writeln(row);
}

Future<void> until(String label, Future<bool> Function() predicate) async {
  final deadline = DateTime.now().add(const Duration(seconds: 35));
  while (DateTime.now().isBefore(deadline)) {
    if (await predicate()) return;
    await Future<void>.delayed(const Duration(milliseconds: 100));
  }
  throw StateError('Timed out: $label');
}

Future<void> rasterizedFrame() async {
  final requested = Timeline.now;
  final completed = Completer<void>();
  void onTimings(List<FrameTiming> timings) {
    if (!completed.isCompleted &&
        timings.any((timing) =>
            timing.timestampInMicroseconds(FramePhase.buildStart) >= requested)) {
      completed.complete();
    }
  }
  WidgetsBinding.instance.addTimingsCallback(onTimings);
  WidgetsBinding.instance.scheduleFrame();
  try {
    await completed.future.timeout(const Duration(seconds: 15));
  } finally {
    WidgetsBinding.instance.removeTimingsCallback(onTimings);
  }
}

Future<Map<String, dynamic>> childState(String id) async {
  final file = File('${proof.path}/window-$id.json');
  return await file.exists()
      ? jsonDecode(await file.readAsString()) as Map<String, dynamic>
      : {};
}

Future<void> command(String id, String action) => save('command-$id.json', {
  'id': DateTime.now().microsecondsSinceEpoch,
  'action': action,
});
Future<void> childLoop() async {
  await until('child host', () async => DesktopHost.instance != null);
  final host = DesktopHost.instance!;
  host.errors.listen(
    (error) =>
        record('child-error', {'window': host.windowId, 'error': '$error'}),
  );
  await until('user session shell', () async {
    var ready = false;
    void visit(Element element) {
      if (element.widget is PaseoShell) ready = true;
      element.visitChildren(visit);
    }

    WidgetsBinding.instance.rootElement?.visitChildren(visit);
    return ready;
  });
  await WidgetsBinding.instance.endOfFrame;
  Object? lastCommand;
  while (true) {
    if (host.integrations.lifecycleReady) {
      await save('window-${host.windowId}.json', {
        'id': host.windowId,
        'visible': await windowManager.isVisible(),
        'lifecycleReady': true,
      });
      final file = File('${proof.path}/command-${host.windowId}.json');
      if (await file.exists()) {
        final request =
            jsonDecode(await file.readAsString()) as Map<String, dynamic>;
        if (request['id'] != lastCommand) {
          lastCommand = request['id'];
          if (request['action'] == 'close') {
            record('child-native-close-request', host.windowId);
            await windowManager.close();
            record('child-native-close-returned', host.windowId);
          }
          if (request['action'] == 'newWindow') {
            final opened = await host.openNew();
            await save('opened-${host.windowId}.json', {'windowId': opened.windowId});
          }
        }
      }
    }
    await Future<void>.delayed(const Duration(milliseconds: 100));
  }
}

Future<void> vimScenario() async {
  late VimEngine engine;
  var observed = '';
  var saved = false;
  engine = await VimEngine.start(
    text: 'alpha\nbeta',
    onText: (text) => observed = text,
    onSave: () async {
      await File('${proof.path}/vim-saved.txt').writeAsString(engine.text);
      saved = true;
      return true;
    },
    onClose: () async {},
  );
  try {
    require(
      engine.ready && engine.error == null,
      'Native Vim engine did not initialize',
    );
    await engine.input('dd');
    await until('native Vim delete line', () async => engine.text == 'beta');
    await engine.input('u');
    await until('native Vim undo', () async => engine.text == 'alpha\nbeta');
    await engine.input('A');
    await until('native Vim insert mode', () async => engine.acceptsText);
    await engine.applyFlutterValue(
      const TextEditingValue(
        text: 'alpha 한글\nbeta',
        selection: TextSelection.collapsed(offset: 8),
      ),
    );
    await engine.input('<Esc>');
    await until(
      'native Vim unicode insertion',
      () async => engine.text == 'alpha 한글\nbeta' && observed == engine.text,
    );
    await engine.input(':w<CR>');
    await until('native Vim save bridge', () async => saved);
    require(
      await File('${proof.path}/vim-saved.txt').readAsString() ==
          'alpha 한글\nbeta',
      'Native Vim save lost unicode or line state',
    );
    require(engine.error == null, 'Native Vim runtime failed: ${engine.error}');
    await rasterizedFrame();
    record('native-vim-delete-undo-unicode-save', {
      'text': engine.text,
      'mode': engine.mode,
    });
  } finally {
    await engine.close();
  }
}

Future<void> toolDetailScenario() async {
  final originalSize = await windowManager.getSize();
  await windowManager.setSize(const Size(412, 915));
  Element? shell;
  await until('compact session shell', () async {
    void visit(Element element) {
      if (element.widget is PaseoShell &&
          MediaQuery.sizeOf(element).width == 412) {
        shell = element;
      }
      element.visitChildren(visit);
    }
    WidgetsBinding.instance.rootElement?.visitChildren(visit);
    return shell != null;
  });
  const command = 'bash running: python3 "/home/user/project with spaces/scripts/'
      'validate_audio_latency_and_bluetooth_lifecycle.py" '
      '--target android --variant release --output /tmp/validation-results.json\n'
      'Waiting for the build to finish';
  final detail = {'type': 'plain_text', 'label': command, 'text': ''};
  final closed = showToolCallSheet(
    shell!,
    icon: resolveToolCallIcon('wait', detail),
    displayName: 'Wait',
    detail: detail,
  );
  await until('full wait command in the opened sheet', () async {
    var visible = false;
    void visit(Element element) {
      if (element.widget case Text(data: command)) {
        visible = ModalRoute.of(element)?.animation?.isCompleted == true;
      }
      element.visitChildren(visit);
    }
    WidgetsBinding.instance.rootElement?.visitChildren(visit);
    return visible;
  });
  await rasterizedFrame();
  record('label-only-wait-command-sheet');
  await until('capturing wait command sheet', () async =>
      File('${proof.path}/captured-label-only-wait-command-sheet').exists());
  Navigator.of(shell!).pop();
  await closed;
  await windowManager.setSize(originalSize);
}

Future<void> rootScenario() async {
  await until('primary host', () async => DesktopHost.instance != null);
  final host = DesktopHost.instance!;
  final failures = <Object>[];
  host.errors.listen((error) {
    failures.add(error);
    record('desktop-error', '$error');
  });
  await until('first visible session shell in the primary engine', () async {
    final windows = await WindowController.getAll();
    return windows.length == 1 &&
        windows.single.windowId == host.windowId &&
        (await childState(host.windowId))['visible'] == true;
  });
  require(failures.isEmpty, 'Desktop startup failed: $failures');
  require(host.isPrimary && await windowManager.isVisible(),
      'Primary engine did not show the first user window');
  final first = host.controller;
  final started = await host.daemon.status();
  require(
    started['status'] == 'running',
    'Desktop coordinator failed to start native daemon',
  );
  await save('started.json', started);
  record('native-single-primary-user-engine', {
    'primary': host.windowId,
    'engineCount': (await WindowController.getAll()).length,
    'daemon': started,
  });
  await until('cold-start project routed after shell and connection readiness',
      () async => host.store.hosts.any((connection) =>
          connection.projects.any((project) => project['projectRootPath'] == proof.path)));
  record('primary-cold-start-project-route', proof.path);
  await vimScenario();
  await until(
    'capturing initial rendered window',
    () =>
        File('${proof.path}/captured-native-vim-delete-undo-unicode-save')
            .exists(),
  );
  await toolDetailScenario();
  if (Platform.environment['PASEO_SMOKE_PANEL'] != '0') {
    await windowManager.close();
    await until('primary window hidden to tray',
        () async => !await windowManager.isVisible());
    require((await WindowController.getAll()).length == 1 &&
        (await host.daemon.status())['pid'] == started['pid'],
        'Closing the sole primary destroyed its engine or daemon');
    await host.show();
    require(await windowManager.isVisible(), 'Primary tray window did not restore');
    record('primary-tray-hide-and-restore');
  }
  var second = await host.openNew();
  await until(
    'second actual visible user engine',
    () async => (await childState(second.windowId))['visible'] == true,
  );
  final removeGuard = host.store.registerCloseGuard(
    'smoke:unsaved-document', () async => false,
  );
  await host.close();
  require(await windowManager.isVisible(),
      'Closing the primary ignored a dirty-document cancellation');
  await host.quit();
  require(await windowManager.isVisible() &&
      (await host.daemon.status())['pid'] == started['pid'],
      'Cancelled Quit hid the primary or stopped the daemon');
  removeGuard();
  record('primary-close-and-quit-cancellation');
  await command(first.windowId, 'close');
  await until(
    'primary retained but hidden while second window stays open',
    () async => (await childState(first.windowId))['visible'] == false,
  );
  require((await childState(second.windowId))['visible'] == true &&
      (await WindowController.getAll()).length == 2,
      'Closing the primary destroyed another window or the owning engine');
  await command(second.windowId, 'newWindow');
  final opened = File('${proof.path}/opened-${second.windowId}.json');
  await until('secondary New Window restores the retained primary', opened.exists);
  final reopenedId = (jsonDecode(await opened.readAsString()) as Map)['windowId'];
  require(reopenedId == first.windowId &&
      (await WindowController.getAll()).length == 2 &&
      await windowManager.isVisible(),
      'New Window failed to reuse the retained primary');
  await command(second.windowId, 'close');
  await until(
    'secondary engine destroyed while primary continues rendering',
    () async => (await WindowController.getAll()).length == 1,
  );
  require(await windowManager.isVisible(), 'Secondary close hid the primary');
  await rasterizedFrame();
  record('primary-reuse-and-secondary-teardown');
  await until('capturing the primary after secondary teardown',
      () => File('${proof.path}/captured-primary-reuse-and-secondary-teardown').exists());
  second = await host.openNew();
  await until('replacement secondary visible',
      () async => (await childState(second.windowId))['visible'] == true);
  await command(first.windowId, 'close');
  await until('primary hidden again',
      () async => (await childState(first.windowId))['visible'] == false);
  require(
    (await host.daemon.status())['pid'] == started['pid'],
    'Closing one window stopped or replaced the daemon',
  );
  record('multiwindow-close-retains-daemon', {'retained': second.windowId});
  await until(
    'capturing the retained window before hiding',
    () =>
        File('${proof.path}/captured-multiwindow-close-retains-daemon')
            .exists(),
  );
  if (Platform.environment['PASEO_SMOKE_PANEL'] == '0') {
    record('expect-no-panel-quit');
    await command(second.windowId, 'close');
    return;
  }
  await command(second.windowId, 'close');
  await until(
    'last user engine hidden to tray',
    () async => (await childState(second.windowId))['visible'] == false,
  );
  require(
    (await WindowController.getAll()).any(
      (value) => value.windowId == second.windowId,
    ),
    'Tray close destroyed the retained engine',
  );
  require(
    (await host.daemon.status())['pid'] == started['pid'],
    'Tray close stopped or replaced the daemon',
  );
  record('last-window-hidden-to-tray');
  await until(
    'capturing the hidden window',
    () => File('${proof.path}/captured-last-window-hidden-to-tray').exists(),
  );
  await save('panel-command.json', {'action': 'activate'});
  await until(
    'real tray activation restores user engine',
    () async => (await childState(second.windowId))['visible'] == true,
  );
  require(failures.isEmpty, 'Desktop lifecycle failed: $failures');
  record('tray-activation-restores-same-engine', second.windowId);
  await until(
    'restoring the same rendered window',
    () =>
        File('${proof.path}/captured-tray-activation-restores-same-engine')
            .exists(),
  );
  if (Platform.environment['PASEO_SMOKE_LOGOUT'] == '1') {
    await command(second.windowId, 'close');
    await until(
      'hidden before logout',
      () async => (await childState(second.windowId))['visible'] == false,
    );
    record('expect-session-logout-quit');
    await save('panel-command.json', {'action': 'logout'});
  } else {
    record('expect-real-tray-menu-quit');
    await save('panel-command.json', {'action': 'quit'});
  }
}

Future<void> main(List<String> arguments) async {
  await runZonedGuarded(
    () async {
      await production.main(arguments);
      if (arguments.firstOrNull == 'multi_window') {
        await childLoop();
      } else {
        unawaited(childLoop());
        await rootScenario();
      }
    },
    (Object error, StackTrace stack) {
      record('FAIL', {'error': '$error', 'stack': '$stack'});
      exit(1);
    },
  );
}
