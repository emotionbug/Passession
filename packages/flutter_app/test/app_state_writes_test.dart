import 'dart:convert';
import 'dart:io';

import 'package:flutter_test/flutter_test.dart';
import 'package:path_provider_platform_interface/path_provider_platform_interface.dart';
import 'package:paseo_sessions/core/app_store.dart';
import 'package:paseo_sessions/core/json.dart';

class _SupportDirectory extends PathProviderPlatform {
  _SupportDirectory(this.path);
  final String path;
  @override
  Future<String?> getApplicationSupportPath() async => path;
}

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();
  late Directory directory;
  late PathProviderPlatform original;
  setUp(() async {
    directory = await Directory.systemTemp.createTemp('paseo-state-writes-test-');
    original = PathProviderPlatform.instance;
    PathProviderPlatform.instance = _SupportDirectory(directory.path);
  });
  tearDown(() async {
    PathProviderPlatform.instance = original;
    await directory.delete(recursive: true);
  });

  AppStore store() => AppStore(connectHosts: false, discoverLocalHosts: false);
  Future<File> stateFile() async {
    final dir = Directory('${directory.path}/paseo-native');
    await dir.create(recursive: true);
    return File('${dir.path}/state.json');
  }

  test('a save with nothing changed leaves state.json alone; a change still lands', () async {
    final app = store();
    await app.initialize();
    app.preferences.locale = 'ja';
    await app.savePreferences();
    final file = File('${directory.path}/paseo-native/state.json');
    // Any write would replace this distinctive formatting.
    final pretty = const JsonEncoder.withIndent('  ').convert(jsonDecode(await file.readAsString()));
    await file.writeAsString(pretty);

    await app.savePreferences();
    expect(await file.readAsString(), pretty);

    app.preferences.locale = 'ko';
    await app.savePreferences();
    final saved = await file.readAsString();
    expect(saved, isNot(pretty));
    expect(jsonMap(jsonDecode(saved)).object('preferences')['language'], 'ko');
    await app.disposeAsync();
    // Disposing an unchanged store writes nothing either.
    expect(await file.readAsString(), saved);
  });

  test('saved state restores preferences and disposing keeps the stored values', () async {
    final file = await stateFile();
    // The shape this store writes without connected hosts.
    const saved = {
      'version': 1,
      'preferences': {'language': 'fr'},
      'hosts': <Object>[],
      'suppressedRoutes': <Object>[],
    };
    await file.writeAsString(jsonEncode(saved));
    final app = store();
    await app.initialize();
    expect(app.storageError, isNull);
    expect(app.preferences.locale, 'fr');
    await app.disposeAsync();
    expect(jsonDecode(await file.readAsString()), saved);
  });

  test('corrupt or newer state is reported and never replaced by defaults', () async {
    final file = await stateFile();
    for (final (contents, error) in [
      ('{broken existing state', isA<FormatException>()),
      ('{"version":1,"preferences":[]}', isA<FormatException>()),
      ('{"version":2,"preferences":{"language":"fr"}}', isA<StateError>()),
    ]) {
      await file.writeAsString(contents);
      final app = store();
      await app.initialize();
      expect(app.storageError, error);
      app.preferences.locale = 'ko';
      await app.savePreferences();
      await app.disposeAsync();
      expect(await file.readAsString(), contents);
    }
  });
}
