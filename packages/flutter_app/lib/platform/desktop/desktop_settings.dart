import 'dart:convert';
import 'dart:io';

import 'package:path/path.dart' as p;
import 'package:path_provider/path_provider.dart';

import '../../core/json.dart';

/// One process-wide settings owner; secondary engines forward mutations to it.
class DesktopSettingsStore {
  late File _file;
  JsonMap _document = {};
  Future<void> _pending = Future.value();

  static JsonMap defaults() => {
    'releaseChannel': 'stable',
    'notifications': {'playSound': true},
    'daemon': {'manageBuiltInDaemon': true, 'keepRunningAfterQuit': false},
  };

  JsonMap get value => jsonMap(jsonDecode(jsonEncode(_document['settings'])));
  bool get managed => value.object('daemon').flag('manageBuiltInDaemon', true);
  bool get keepRunning => value.object('daemon').flag('keepRunningAfterQuit');
  bool get playSound => value.object('notifications').flag('playSound', true);
  String get releaseChannel => value.text('releaseChannel', 'stable');

  Future<void> initialize() async {
    final support = await getApplicationSupportDirectory();
    _file = File(p.join(support.path, 'desktop-settings.json'));
    if (await _file.exists()) {
      _document = jsonMap(jsonDecode(await _file.readAsString()));
    }
    final settings = defaults();
    _merge(settings, jsonMap(_document['settings']));
    _document = {..._document, 'version': 1, 'settings': settings};
    await _save();
  }

  Future<JsonMap> patch(JsonMap patch) async {
    final settings = value;
    _merge(settings, patch);
    _document['settings'] = settings;
    await _save();
    return value;
  }

  void _merge(JsonMap settings, JsonMap patch) {
    if (patch['releaseChannel'] == 'stable' ||
        patch['releaseChannel'] == 'beta') {
      settings['releaseChannel'] = patch['releaseChannel'];
    }
    for (final section in ['notifications', 'daemon']) {
      final source = jsonMap(patch[section]);
      final target = jsonMap(settings[section]);
      final keys = section == 'notifications'
          ? ['playSound']
          : ['manageBuiltInDaemon', 'keepRunningAfterQuit'];
      for (final key in keys) {
        if (source[key] is bool) target[key] = source[key];
      }
      settings[section] = target;
    }
  }

  Future<void> _save() {
    final bytes = jsonEncode(_document);
    final operation = _pending.catchError((Object _) {}).then((_) async {
      await _file.parent.create(recursive: true);
      final temporary = File('${_file.path}.$pid.tmp');
      await temporary.writeAsString(bytes, flush: true);
      await temporary.rename(_file.path);
    });
    _pending = operation;
    return operation;
  }
}
