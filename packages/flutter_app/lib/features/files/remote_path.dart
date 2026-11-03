import 'package:path/path.dart' as p;

// The host's path style can differ from the native client's OS.
final _windowsDrive = RegExp(r'^[A-Za-z]:');
p.Context _remoteStyle(String path) =>
    _windowsDrive.hasMatch(path) || path.startsWith('\\\\')
    ? p.windows
    : p.posix;

String remoteBasename(String path, {String? cwd}) =>
    _remoteStyle(cwd ?? path).basename(path);
String remoteParent(String path) => _remoteStyle(path).dirname(path);
String remoteAbsolute(String cwd, String path) {
  final style = _remoteStyle(cwd);
  return style.normalize(style.isAbsolute(path) ? path : style.join(cwd, path));
}

String remoteRelative(String cwd, String path) {
  final style = _remoteStyle(cwd);
  return style.isAbsolute(path)
      ? style.relative(path, from: cwd)
      : style.normalize(path);
}
