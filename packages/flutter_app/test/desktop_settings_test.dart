import 'dart:convert';
import 'dart:io';

import 'package:flutter_test/flutter_test.dart';
import 'package:path_provider_platform_interface/path_provider_platform_interface.dart';
import 'package:paseo_sessions/platform/desktop/desktop_settings.dart';

class _SupportDirectory extends PathProviderPlatform {
  _SupportDirectory(this.directory);
  final String directory;

  @override
  Future<String?> getApplicationSupportPath() async => directory;
}

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();

  test('stored settings are kept, defaults fill gaps and patches persist', () async {
    final directory = await Directory.systemTemp.createTemp('paseo-settings-test-');
    final original = PathProviderPlatform.instance;
    PathProviderPlatform.instance = _SupportDirectory(directory.path);
    try {
      await File('${directory.path}/desktop-settings.json').writeAsString(
        jsonEncode({
          'version': 1,
          'settings': {
            'releaseChannel': 'beta',
            'daemon': {'manageBuiltInDaemon': false, 'keepRunningAfterQuit': true},
          },
        }),
      );
      final settings = DesktopSettingsStore();
      await settings.initialize();
      expect(settings.managed, isFalse);
      expect(settings.releaseChannel, 'beta');
      expect(settings.keepRunning, isTrue);
      expect(settings.playSound, isTrue);

      await settings.patch({
        'daemon': {'keepRunningAfterQuit': false},
      });
      final reopened = DesktopSettingsStore();
      await reopened.initialize();
      expect(reopened.keepRunning, isFalse);
      expect(reopened.managed, isFalse);
      expect(reopened.releaseChannel, 'beta');
    } finally {
      PathProviderPlatform.instance = original;
      await directory.delete(recursive: true);
    }
  });
}
