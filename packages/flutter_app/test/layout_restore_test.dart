import 'dart:convert';
import 'dart:io';

import 'package:flutter_test/flutter_test.dart';
import 'package:path_provider_platform_interface/path_provider_platform_interface.dart';
import 'package:paseo_sessions/core/app_store.dart';
import 'package:paseo_sessions/core/target.dart';

class _SupportDirectory extends PathProviderPlatform {
  _SupportDirectory(this.path);
  final String path;
  @override
  Future<String?> getApplicationSupportPath() async => path;
}

const _files = PanelTarget(
  kind: 'files',
  serverId: 'srv',
  workspaceId: 'ws',
  cwd: '/repo',
);
const _changes = PanelTarget(
  kind: 'changes_tree',
  serverId: 'srv',
  workspaceId: 'ws',
  cwd: '/repo',
);
const _agent = PanelTarget(
  kind: 'agent',
  serverId: 'srv',
  workspaceId: 'ws',
  cwd: '/repo',
  id: 'agent',
);
const _log = PanelTarget(
  kind: 'execution_log',
  serverId: 'srv',
  workspaceId: 'ws',
  cwd: '/repo',
  id: 'log',
);

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();
  late Directory directory;
  late PathProviderPlatform original;
  setUp(() async {
    directory = await Directory.systemTemp.createTemp(
      'paseo-layout-restore-test-',
    );
    original = PathProviderPlatform.instance;
    PathProviderPlatform.instance = _SupportDirectory(directory.path);
  });
  tearDown(() async {
    PathProviderPlatform.instance = original;
    await directory.delete(recursive: true);
  });

  Future<AppStore> open(Map<String, dynamic> layout) async {
    final dir = Directory('${directory.path}/paseo-native');
    await dir.create(recursive: true);
    await File('${dir.path}/state.json').writeAsString(
      jsonEncode({
        'version': 1,
        'preferences': <String, dynamic>{},
        'hosts': <dynamic>[],
        'layout': layout,
      }),
    );
    final store = AppStore(connectHosts: false, discoverLocalHosts: false);
    await store.initialize();
    return store;
  }

  test('a saved Explorer restores exactly as saved', () async {
    final both = await open({
      'version': 2,
      'tabs': [_files.toJson(), _changes.toJson()],
      'tree': {'kind': 'pane', 'id': 'main', 'tabs': <String>[]},
      'explorerTabs': [_files.key, _changes.key],
      'selectedExplorer': _files.key,
    });
    expect(both.explorerTabKeys, [_files.key, _changes.key]);
    expect(both.selectedExplorerKey, _files.key);
    await both.disposeAsync();

    final filesOnly = await open({
      'version': 2,
      'tabs': [_files.toJson()],
      'tree': {'kind': 'pane', 'id': 'main', 'tabs': <String>[]},
      'explorerTabs': [_files.key],
      'selectedExplorer': _files.key,
    });
    expect(filesOnly.explorerTabKeys, [_files.key]);
    expect(filesOnly.selectedExplorerKey, _files.key);
    await filesOnly.disposeAsync();
  });

  test('removed surfaces are dropped and supported tabs keep their pane and selection', () async {
    const browser = PanelTarget(
      kind: 'browser',
      serverId: 'srv',
      workspaceId: 'ws',
      cwd: '/repo',
      id: 'b',
    );
    const plugin = PanelTarget(
      kind: 'plugin',
      serverId: 'srv',
      workspaceId: 'ws',
      cwd: '/repo',
      id: 'p',
    );
    const terminal = PanelTarget(
      kind: 'terminal',
      serverId: 'srv',
      workspaceId: 'ws',
      cwd: '/repo',
      id: 't',
    );
    final store = await open({
      'version': 2,
      'tabs': [
        for (final t in [browser, _agent, plugin, terminal, _log]) t.toJson(),
      ],
      'tree': {
        'kind': 'pane',
        'id': 'main',
        'tabs': [browser.key, _agent.key, plugin.key, terminal.key, _log.key],
        'selected': _log.key,
      },
    });
    expect(store.tabs.map((t) => t.key), [_agent.key, _log.key]);
    final pane = store.layout.panes.single;
    expect(pane.tabKeys, [_agent.key, _log.key]);
    expect(pane.selectedKey, _log.key);
    await store.disposeAsync();
  });
}
