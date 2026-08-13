import 'dart:async';

import 'package:buzz/features/pairing/pairing_provider.dart';
import 'package:buzz/features/settings/settings_page.dart';
import 'package:buzz/shared/auth/auth.dart';
import 'package:buzz/shared/relay/relay.dart';
import 'package:buzz/shared/theme/theme.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:nostr/nostr.dart' as nostr;
import 'package:shared_preferences/shared_preferences.dart';

import '../../helpers/widget_helpers.dart';

void main() {
  testWidgets('waits for a resumed frame before navigating after auth', (
    tester,
  ) async {
    final authorization = Completer<bool>();
    final pairing = _PairingNotifier(authorization.future);
    SharedPreferences.setMockInitialValues({});
    final prefs = await SharedPreferences.getInstance();

    await tester.pumpWidget(
      WidgetHelpers.testable(
        overrides: [
          relayConfigProvider.overrideWith(_RelayConfigNotifier.new),
          authProvider.overrideWith(_AuthNotifier.new),
          pairingProvider.overrideWith(() => pairing),
          savedPrefsProvider.overrideWithValue(prefs),
        ],
        child: SettingsPage(
          profileHeader: const SizedBox.shrink(),
          identityRecoveryPageBuilder: (_) =>
              const Scaffold(body: Text('Identity recovery')),
        ),
      ),
    );
    await tester.pump();

    await tester.tap(find.text('Send identity to desktop'));
    await tester.pump();
    tester.binding.handleAppLifecycleStateChanged(AppLifecycleState.inactive);
    authorization.complete(true);
    await tester.pump();

    expect(find.text('Identity recovery'), findsNothing);

    tester.binding.handleAppLifecycleStateChanged(AppLifecycleState.resumed);
    await tester.pump();
    await tester.pump();
    await tester.pumpAndSettle();

    expect(find.text('Identity recovery'), findsOneWidget);
  });
}

class _AuthNotifier extends AuthNotifier {
  @override
  Future<AuthState> build() async => AuthState(
    status: AuthStatus.authenticated,
    community: Community(
      id: 'community',
      name: 'Test',
      relayUrl: 'https://relay.test',
      nsec: _RelayConfigNotifier.nsec,
      addedAt: DateTime.utc(2026),
    ),
  );
}

class _RelayConfigNotifier extends RelayConfigNotifier {
  static final nsec = nostr.Keys(
    '1111111111111111111111111111111111111111111111111111111111111111',
  ).nsec;

  @override
  RelayConfig build() => RelayConfig(baseUrl: 'https://relay.test', nsec: nsec);
}

class _PairingNotifier extends PairingNotifier {
  _PairingNotifier(this.authorization);

  final Future<bool> authorization;

  @override
  PairingState build() => const PairingState();

  @override
  Future<bool> authorizeIdentityExport({required Community community}) =>
      authorization;

  @override
  void reset() {}
}
