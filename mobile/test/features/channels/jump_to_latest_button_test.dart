import 'package:buzz/features/channels/jump_to_latest_button.dart';
import 'package:buzz/shared/theme/theme.dart';
import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  testWidgets('uses the native glass control on iOS', (tester) async {
    debugDefaultTargetPlatformOverride = TargetPlatform.iOS;
    try {
      await tester.pumpWidget(
        MaterialApp(
          theme: AppTheme.light(),
          home: Scaffold(body: JumpToLatestButton(onPressed: () {})),
        ),
      );

      final nativeView = tester.widget<UiKitView>(find.byType(UiKitView));
      expect(nativeView.viewType, 'buzz/jump_to_latest_glass');
      expect(
        find.byKey(const ValueKey('channel-jump-to-latest-ios-glass')),
        findsOneWidget,
      );
    } finally {
      debugDefaultTargetPlatformOverride = null;
    }
  });
}
