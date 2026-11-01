import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:intl/intl.dart';

import '../core/json.dart';

/// English first: Flutter's locale resolution falls back to the first entry.
const supportedPaseoLocales = [Locale('en'), Locale('ko')];

/// Values shown by the language setting.
const paseoLanguageOptions = ['system', 'en', 'ko'];

/// Canonical language setting for a stored `language` value: `system`, `en`
/// or `ko`. Region/script variants (`ko-KR`, `en_US`) keep their language;
/// any other tag, including catalogs that were removed, resolves to English.
/// The stored value itself is never rewritten here.
String paseoLanguagePreference(Object? stored) {
  if (stored is! String) return 'system';
  final value = stored.trim();
  if (value.isEmpty || value == 'system' || value == 'auto') return 'system';
  return value.split(RegExp('[-_]')).first.toLowerCase() == 'ko' ? 'ko' : 'en';
}

/// Explicit app locale for a stored `language` value; null follows the device.
Locale? paseoLocaleFor(Object? stored) =>
    switch (paseoLanguagePreference(stored)) {
      'system' => null,
      final tag => Locale(tag),
    };

/// Catalog for a resolved locale; anything but Korean uses English.
String paseoCatalogTag(Locale locale) =>
    locale.languageCode.toLowerCase() == 'ko' ? 'ko' : 'en';

String tr(
  BuildContext context,
  String key, {
  Map<String, Object?> args = const {},
}) =>
    Localizations.of<PaseoLocalizations>(
      context,
      PaseoLocalizations,
    )?.text(key, args) ??
    key;

class PaseoLocalizations {
  PaseoLocalizations(this.locale, this._catalog, this._english);
  final Locale locale;
  final JsonMap _catalog;
  final JsonMap _english;
  static final Map<String, Future<JsonMap>> _catalogs = {};

  static Future<JsonMap> _load(String tag) => _catalogs.putIfAbsent(
    tag,
    () async => jsonMap(
      jsonDecode(await rootBundle.loadString('assets/locales/$tag.json')),
    ),
  );

  static Future<PaseoLocalizations> load(Locale locale) async {
    final tag = paseoCatalogTag(locale);
    final catalogs = await Future.wait([_load(tag), _load('en')]);
    return PaseoLocalizations(locale, catalogs[0], catalogs[1]);
  }

  static String? _lookup(JsonMap catalog, String key) {
    Object? current = catalog;
    for (final part in key.split('.')) {
      if (current is! JsonMap) return null;
      current = current[part];
    }
    return current is String ? current : null;
  }

  String text(String key, Map<String, Object?> args) {
    var pluralKey = key;
    final count = args['count'];
    if (count is num) {
      final suffix = Intl.pluralLogic(
        count,
        locale: locale.toLanguageTag(),
        zero: 'zero',
        one: 'one',
        two: 'two',
        few: 'few',
        many: 'many',
        other: 'other',
      );
      pluralKey = '${key}_$suffix';
    }
    final template =
        _lookup(_catalog, pluralKey) ??
        _lookup(_catalog, key) ??
        _lookup(_english, pluralKey) ??
        _lookup(_english, key) ??
        key;
    return template.replaceAllMapped(
      RegExp(r'\{\{\s*([^{}]+?)\s*\}\}'),
      (match) => args[match[1]]?.toString() ?? match[0]!,
    );
  }

  static const LocalizationsDelegate<PaseoLocalizations> delegate =
      _PaseoLocalizationsDelegate();
}

class _PaseoLocalizationsDelegate
    extends LocalizationsDelegate<PaseoLocalizations> {
  const _PaseoLocalizationsDelegate();
  @override
  bool isSupported(Locale locale) => supportedPaseoLocales.any(
    (item) => item.languageCode == locale.languageCode,
  );
  @override
  Future<PaseoLocalizations> load(Locale locale) =>
      PaseoLocalizations.load(locale);
  @override
  bool shouldReload(_PaseoLocalizationsDelegate old) => false;
}
