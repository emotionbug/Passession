import 'dart:convert';
import 'dart:io';

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:path_provider_platform_interface/path_provider_platform_interface.dart';
import 'package:paseo_sessions/app.dart';
import 'package:paseo_sessions/core/app_store.dart';
import 'package:paseo_sessions/core/json.dart';
import 'package:paseo_sessions/features/settings/app_preferences.dart';
import 'package:paseo_sessions/features/settings/settings_forms.dart';
import 'package:paseo_sessions/ui/localization.dart';

class _SupportDirectory extends PathProviderPlatform {
  _SupportDirectory(this.path);
  final String path;
  @override
  Future<String?> getApplicationSupportPath() async => path;
}

AppStore _store() => AppStore(connectHosts: false, discoverLocalHosts: false);

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();
  late Directory directory;
  late PathProviderPlatform original;
  setUp(() async {
    directory = await Directory.systemTemp.createTemp('paseo-localization-test-');
    original = PathProviderPlatform.instance;
    PathProviderPlatform.instance = _SupportDirectory(directory.path);
  });
  tearDown(() async {
    PathProviderPlatform.instance = original;
    await directory.delete(recursive: true);
  });

  /// Catalog futures are cached statically; completing them on real time keeps
  /// the delegate load synchronous enough for fake-async widget pumps.
  Future<void> warmCatalogs(WidgetTester tester) async {
    await tester.runAsync(() => Future.wait([
      PaseoLocalizations.load(const Locale('en')),
      PaseoLocalizations.load(const Locale('ko')),
    ]));
  }

  /// `PaseoLocalizations.load` awaits catalog futures that [warmCatalogs]
  /// completed on real time. Dart delivers an already-completed future to a new
  /// listener through a microtask scheduled in the zone that created the
  /// future, and fake-async pumps never drain that real queue, so the app's
  /// Localizations would keep waiting. The first pump builds any pending locale
  /// change, which starts the delegate load; loading a catalog on real time
  /// then queues behind those deliveries, so once it completes they have run;
  /// the final pump applies the loaded resources.
  Future<void> settleLocalizations(WidgetTester tester) async {
    await tester.pump();
    await tester.runAsync(() => PaseoLocalizations.load(const Locale('en')));
    await tester.pumpAndSettle();
  }

  Future<AppStore> reopenedWith(WidgetTester tester, Map<String, Object?> preferences) async =>
      (await tester.runAsync(() async {
        final first = _store();
        await first.initialize();
        preferences.forEach((key, value) => first.preferences[key] = value);
        await first.savePreferences();
        await first.disposeAsync();
        final reopened = _store();
        await reopened.initialize();
        return reopened;
      }))!;

  SettingsSelectRow<String> languageRow(WidgetTester tester) =>
      tester.widget<SettingsSelectRow<String>>(find.byWidgetPredicate(
        (w) => w is SettingsSelectRow<String> && w.options.any((o) => o.$1 == 'system'),
      ));

  test('stored language values resolve to system, English or Korean', () {
    for (final removed in ['ar', 'es', 'fr', 'ja', 'pt-BR', 'ru', 'zh-CN', 'zh', 'de', 'english']) {
      expect(paseoLanguagePreference(removed), 'en', reason: removed);
      expect(paseoLocaleFor(removed), const Locale('en'), reason: removed);
    }
    for (final korean in ['ko', 'ko-KR', 'ko_KR', 'KO', ' ko-Kore-KR ']) {
      expect(paseoLanguagePreference(korean), 'ko', reason: korean);
      expect(paseoLocaleFor(korean), const Locale('ko'), reason: korean);
    }
    for (final english in ['en', 'en-US', 'en_GB']) {
      expect(paseoLanguagePreference(english), 'en', reason: english);
    }
    for (final system in ['system', 'auto', '', null, 3, true]) {
      expect(paseoLanguagePreference(system), 'system', reason: '$system');
      expect(paseoLocaleFor(system), isNull, reason: '$system');
    }
  });

  test('catalog loading never requests a removed catalog', () async {
    expect(supportedPaseoLocales, [const Locale('en'), const Locale('ko')]);
    final french = await PaseoLocalizations.load(const Locale('fr'));
    final chinese = await PaseoLocalizations.load(const Locale('zh', 'CN'));
    final korean = await PaseoLocalizations.load(const Locale('ko', 'KR'));
    expect(french.text('settings.general.language.label', const {}), 'Language');
    expect(chinese.text('settings.general.language.label', const {}), 'Language');
    expect(korean.text('settings.general.language.label', const {}), '언어');
    expect(korean.text('settings.general.language.options.ko', const {}), '한국어');
  });

  testWidgets('a removed persisted language renders English, keeps settings, and the selector offers only System/English/Korean',
      (tester) async {
    await warmCatalogs(tester);
    final store = await reopenedWith(tester, {'language': 'ja', 'theme': 'claude', 'vimKeybindings': true});
    await tester.pumpWidget(PaseoApp(
      store: store,
      home: Scaffold(body: SingleChildScrollView(child: AppPreferenceSettings(store: store))),
    ));
    await settleLocalizations(tester);

    final context = tester.element(find.byType(AppPreferenceSettings));
    expect(Localizations.localeOf(context), const Locale('en'));
    var row = languageRow(tester);
    expect(row.label, 'Language');
    expect(row.value, 'en');
    expect(row.options, [('system', 'System'), ('en', 'English'), ('ko', 'Korean')]);
    // Resolution is read-only: nothing about the stored preferences changed.
    expect(store.preferences['language'], 'ja');
    expect(store.preferences['theme'], 'claude');
    expect(store.preferences['vimKeybindings'], isTrue);

    // Selecting runs the real save on real time so its file I/O can finish.
    await tester.runAsync(() async {
      row.onChanged!('ko');
      final file = File('${directory.path}/paseo-native/state.json');
      for (var i = 0; i < 100; i++) {
        try {
          final saved = jsonMap(jsonDecode(await file.readAsString())).object('preferences');
          if (saved['language'] == 'ko') break;
        } on FormatException {
          // Mid-write; read again.
        }
        await Future<void>.delayed(const Duration(milliseconds: 20));
      }
    });
    await settleLocalizations(tester);

    expect(Localizations.localeOf(tester.element(find.byType(AppPreferenceSettings))), const Locale('ko'));
    row = languageRow(tester);
    expect(row.label, '언어');
    expect(row.value, 'ko');
    expect(row.options, [('system', '시스템'), ('en', 'English'), ('ko', '한국어')]);
    expect(store.preferences['theme'], 'claude');
    expect(store.preferences['vimKeybindings'], isTrue);

    await tester.pumpWidget(const SizedBox.shrink());
    await tester.runAsync(store.disposeAsync);
  });

  for (final (language, device, expected) in [
    ('ko-KR', const Locale('en', 'US'), const Locale('ko')),
    ('en-GB', const Locale('ko', 'KR'), const Locale('en')),
    ('system', const Locale('ko', 'KR'), const Locale('ko')),
    ('system', const Locale('fr', 'FR'), const Locale('en')),
    ('system', const Locale('zh', 'CN'), const Locale('en')),
    ('zh-CN', const Locale('ko', 'KR'), const Locale('en')),
  ]) {
    testWidgets('language $language on a $device device resolves to $expected', (tester) async {
      tester.platformDispatcher.localesTestValue = [device];
      addTearDown(tester.platformDispatcher.clearLocalesTestValue);
      await warmCatalogs(tester);
      final store = await tester.runAsync(() async {
        final store = _store();
        await store.initialize();
        store.preferences['language'] = language;
        return store;
      });
      await tester.pumpWidget(PaseoApp(
        store: store!,
        home: Builder(builder: (context) => Text(tr(context, 'settings.general.language.label'))),
      ));
      await settleLocalizations(tester);

      expect(Localizations.localeOf(tester.element(find.byType(Text))), expected);
      expect(find.text(expected.languageCode == 'ko' ? '언어' : 'Language'), findsOneWidget);
      expect(store.preferences['language'], language);

      await tester.pumpWidget(const SizedBox.shrink());
      await tester.runAsync(store.disposeAsync);
    });
  }
}
