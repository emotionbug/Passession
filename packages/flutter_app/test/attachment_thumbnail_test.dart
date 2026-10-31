import 'dart:convert';
import 'dart:typed_data';
import 'dart:ui' as ui;

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:paseo_sessions/features/chat/attachment_pill.dart';
import 'package:paseo_sessions/ui/theme.dart';

const _png =
    'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==';

Widget _thumbnail(Map<String, dynamic> image, {double pixelRatio = 1}) => MaterialApp(
  theme: paseoTheme(brightness: Brightness.dark, preferences: const {}),
  home: MediaQuery(
    data: MediaQueryData(devicePixelRatio: pixelRatio),
    child: Center(
      child: Builder(builder: (context) => AttachmentPill.thumbnail(context, image)),
    ),
  ),
);

Future<void> _loadThumbnail(WidgetTester tester) async {
  await tester.runAsync(() => precacheImage(
    tester.widget<Image>(find.byType(Image)).image,
    tester.element(find.byType(Image)),
  ));
  await tester.pump();
}

Future<String> _imageData(int width, int height) async {
  final recorder = ui.PictureRecorder();
  final canvas = Canvas(recorder);
  canvas.drawRect(
    Rect.fromLTWH(0, 0, width.toDouble(), height.toDouble()),
    Paint()..color = Colors.blue,
  );
  final picture = recorder.endRecording();
  final image = await picture.toImage(width, height);
  picture.dispose();
  final bytes = await image.toByteData(format: ui.ImageByteFormat.png);
  image.dispose();
  return base64Encode(bytes!.buffer.asUint8List());
}

void _fillCache(String prefix) {
  for (var i = 0; i < 32; i++) {
    AttachmentPill.provider({'data': base64Encode(utf8.encode('$prefix-$i'))});
  }
}

void main() {
  testWidgets('a rebuilt image thumbnail keeps showing its picture', (tester) async {
    await tester.pumpWidget(_thumbnail({'data': _png, 'mimeType': 'image/png'}));
    await _loadThumbnail(tester);
    expect(tester.widget<RawImage>(find.byType(RawImage)).image, isNotNull);

    // The composer rebuilds with a fresh attachment map carrying the same payload.
    _fillCache('evict-mounted-thumbnail');
    await tester.pumpWidget(_thumbnail({'data': _png, 'mimeType': 'image/png'}));
    expect(tester.widget<RawImage>(find.byType(RawImage)).image, isNotNull);
  });

  testWidgets('thumbnail follows a changed payload and display pixel ratio',
      (tester) async {
    final landscape = (await tester.runAsync(() => _imageData(800, 400)))!;
    final portrait = (await tester.runAsync(() => _imageData(400, 800)))!;
    final payload = {'data': landscape};
    await tester.pumpWidget(_thumbnail(payload));
    await _loadThumbnail(tester);
    expect(tester.widget<RawImage>(find.byType(RawImage)).image!.width, 96);

    payload['data'] = portrait;
    await tester.pumpWidget(_thumbnail(payload));
    await _loadThumbnail(tester);
    var decoded = tester.widget<RawImage>(find.byType(RawImage)).image!;
    expect(decoded.width, 48);
    expect(decoded.height, 96);

    await tester.pumpWidget(_thumbnail(payload, pixelRatio: 2));
    await _loadThumbnail(tester);
    decoded = tester.widget<RawImage>(find.byType(RawImage)).image!;
    expect(decoded.width, 96);
    expect(decoded.height, 192);
  });

  for (final (width, height, pixelRatio) in [
    (800, 400, 2.0),
    (400, 800, 2.5),
    (600, 600, 1.0),
  ]) {
    testWidgets('$width×$height thumbnail decodes for ${pixelRatio}x cover',
        (tester) async {
      final data = (await tester.runAsync(() => _imageData(width, height)))!;
      await tester.pumpWidget(_thumbnail({'data': data}, pixelRatio: pixelRatio));
      await _loadThumbnail(tester);

      final decoded = tester.widget<RawImage>(find.byType(RawImage)).image!;
      final shortEdge = (48 * pixelRatio).ceil();
      final scale = shortEdge / (width < height ? width : height);
      expect(decoded.width, (width * scale).round());
      expect(decoded.height, (height * scale).round());
      expect(tester.getSize(find.byType(Image)), const Size(48, 48));
    });
  }

  testWidgets('preview uses original resolution after displaying a thumbnail',
      (tester) async {
    final data = (await tester.runAsync(() => _imageData(800, 400)))!;
    final payload = {'data': data};
    await tester.pumpWidget(_thumbnail(payload));
    await _loadThumbnail(tester);

    // The public provider is also used by the composer and timeline lightboxes.
    final original = AttachmentPill.provider(payload)!;
    await tester.pumpWidget(MaterialApp(
      home: Center(child: Image(image: original)),
    ));
    await _loadThumbnail(tester);
    final preview = tester.widget<RawImage>(find.byType(RawImage)).image!;
    expect(preview.width, 800);
    expect(preview.height, 400);
  });

  testWidgets('small images are not upscaled during decode', (tester) async {
    await tester.pumpWidget(_thumbnail({'data': _png}, pixelRatio: 3));
    await _loadThumbnail(tester);
    final decoded = tester.widget<RawImage>(find.byType(RawImage)).image!;
    expect(decoded.width, 1);
    expect(decoded.height, 1);
  });

  test('inline providers retain the 32 most recently used payloads', () {
    _fillCache('count-boundary');
    Map<String, dynamic> payload(int index) =>
        {'data': base64Encode(utf8.encode('count-boundary-$index'))};
    final oldest = AttachmentPill.provider(payload(0));
    final next = AttachmentPill.provider(payload(1));
    // Refresh the first entry so the second becomes the eviction candidate.
    for (var i = 2; i < 32; i++) {
      AttachmentPill.provider(payload(i));
    }
    expect(AttachmentPill.provider(payload(0)), same(oldest));
    AttachmentPill.provider(payload(32));
    expect(AttachmentPill.provider(payload(0)), same(oldest));
    expect(AttachmentPill.provider(payload(1)), isNot(same(next)));
  });

  test('inline cache evicts by retained bytes before reaching 32 entries', () {
    _fillCache('byte-boundary');
    Map<String, dynamic> payload(int marker) {
      final bytes = Uint8List(2 * 1024 * 1024)..[0] = marker;
      return {'data': base64Encode(bytes)};
    }
    final firstPayload = payload(1);
    final secondPayload = payload(2);
    final first = AttachmentPill.provider(firstPayload);
    final second = AttachmentPill.provider(secondPayload);
    expect(AttachmentPill.provider(firstPayload), same(first));
    expect(AttachmentPill.provider(secondPayload), same(second));
    AttachmentPill.provider(payload(3));
    expect(AttachmentPill.provider(secondPayload), same(second));
    expect(AttachmentPill.provider(firstPayload), isNot(same(first)));
  });

  testWidgets('an oversized mounted image survives rebuilds without decoding again',
      (tester) async {
    final data = (await tester.runAsync(() => _imageData(800, 400)))!;
    // PNG decoders stop at IEND; trailing bytes make a valid oversized payload.
    final bytes = Uint8List(5 * 1024 * 1024)..setAll(0, base64Decode(data));
    final oversized = base64Encode(bytes);
    final first = AttachmentPill.provider({'data': oversized});
    expect(AttachmentPill.provider({'data': oversized}), isNot(same(first)));

    await tester.pumpWidget(_thumbnail({'data': oversized}));
    await _loadThumbnail(tester);
    final before = tester.widget<RawImage>(find.byType(RawImage)).image!.clone();
    addTearDown(before.dispose);
    _fillCache('oversized-rebuild');
    await tester.pumpWidget(_thumbnail({'data': oversized}));
    expect(tester.widget<RawImage>(find.byType(RawImage)).image, isNotNull);
    await _loadThumbnail(tester);
    final after = tester.widget<RawImage>(find.byType(RawImage)).image!;
    expect(after.isCloneOf(before), isTrue);
    expect(after.width, 96);
    expect(after.height, 48);
  });
}
