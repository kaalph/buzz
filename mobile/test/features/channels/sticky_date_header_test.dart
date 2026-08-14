import 'package:buzz/features/channels/sticky_date_header.dart';
import 'package:buzz/shared/theme/theme.dart';
import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  testWidgets('uses native glass and updates its date on iOS', (tester) async {
    debugDefaultTargetPlatformOverride = TargetPlatform.iOS;
    final state = ValueNotifier(
      const StickyDateHeaderState(label: 'Yesterday'),
    );
    try {
      await tester.pumpWidget(
        MaterialApp(
          theme: AppTheme.light(),
          home: Scaffold(body: StickyDateHeader(state: state)),
        ),
      );

      var nativeView = tester.widget<UiKitView>(find.byType(UiKitView));
      expect(nativeView.viewType, 'buzz/sticky_date_glass');
      expect(nativeView.creationParams, <String, Object>{'label': 'Yesterday'});
      expect(find.byType(BackdropFilter), findsNothing);

      state.value = const StickyDateHeaderState(label: 'Today');
      await tester.pump();

      nativeView = tester.widget<UiKitView>(find.byType(UiKitView));
      expect(nativeView.creationParams, <String, Object>{'label': 'Today'});
    } finally {
      state.dispose();
      debugDefaultTargetPlatformOverride = null;
    }
  });

  testWidgets('keeps the Flutter date surface on Android', (tester) async {
    debugDefaultTargetPlatformOverride = TargetPlatform.android;
    final state = ValueNotifier(const StickyDateHeaderState(label: 'Today'));
    try {
      await tester.pumpWidget(
        MaterialApp(
          theme: AppTheme.light(),
          home: Scaffold(body: StickyDateHeader(state: state)),
        ),
      );

      expect(find.byType(UiKitView), findsNothing);
      expect(find.byType(BackdropFilter), findsOneWidget);
      expect(find.text('Today'), findsOneWidget);
    } finally {
      state.dispose();
      debugDefaultTargetPlatformOverride = null;
    }
  });
}
