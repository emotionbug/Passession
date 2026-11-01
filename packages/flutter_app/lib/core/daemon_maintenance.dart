import 'json.dart';

/// Server capability that also makes `daemon.update.maintenance` a valid event
/// subscription. Older daemons reject unknown event names, so subscribe only
/// when it is advertised.
const daemonUpdateMaintenanceFeature = 'durableDaemonUpdates';
const daemonUpdateMaintenanceEvent = 'daemon.update.maintenance';

/// The paused operation in a full `server_info` snapshot
/// (`daemonUpdate: {operationId, active: true}`); absent means no pause.
String? daemonMaintenanceFromServerInfo(JsonMap info) {
  final update = info['daemonUpdate'];
  if (update is! Map || update['active'] != true) return null;
  final id = stringValue(update['operationId']);
  return id.isEmpty ? null : id;
}

/// Applies a `daemon.update.maintenance` event to the paused operation.
/// A release only clears the operation it names, so a late release of an
/// older operation never hides a newer pause. Malformed payloads change nothing.
String? applyDaemonMaintenanceEvent(String? current, JsonMap payload) {
  final id = stringValue(payload['operationId']);
  final active = payload['active'];
  if (id.isEmpty || active is! bool) return current;
  if (active) return id;
  return current == id ? null : current;
}
