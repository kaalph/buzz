import 'package:flutter_test/flutter_test.dart';
import 'package:local_auth/local_auth.dart';
import 'package:mocktail/mocktail.dart';
import 'package:buzz/shared/security/sensitive_action_authorizer.dart';

class _MockLocalAuthentication extends Mock implements LocalAuthentication {}

void main() {
  late _MockLocalAuthentication authentication;

  setUp(() {
    authentication = _MockLocalAuthentication();
    when(authentication.isDeviceSupported).thenAnswer((_) async => true);
    when(
      authentication.getAvailableBiometrics,
    ).thenAnswer((_) async => <BiometricType>[BiometricType.face]);
    when(
      () => authentication.authenticate(
        localizedReason: any(named: 'localizedReason'),
        authMessages: any(named: 'authMessages'),
        biometricOnly: any(named: 'biometricOnly'),
        sensitiveTransaction: any(named: 'sensitiveTransaction'),
        persistAcrossBackgrounding: any(named: 'persistAcrossBackgrounding'),
      ),
    ).thenAnswer((_) async => true);
  });

  test('identity authorization retries after system backgrounding', () async {
    final result = await LocalSensitiveActionAuthorizer(
      authentication,
    ).authorizeIdentityAction();

    expect(result, DeviceAuthResult.success);
    verify(
      () => authentication.authenticate(
        localizedReason: 'Confirm sending your Buzz identity to desktop',
        authMessages: any(named: 'authMessages'),
        biometricOnly: false,
        sensitiveTransaction: true,
        persistAcrossBackgrounding: true,
      ),
    ).called(1);
  });

  test('biometric protection requires an enrolled biometric', () async {
    when(
      authentication.getAvailableBiometrics,
    ).thenAnswer((_) async => <BiometricType>[]);

    final result = await LocalSensitiveActionAuthorizer(
      authentication,
    ).authorizeBiometricProtection();

    expect(result, DeviceAuthResult.unavailable);
    verifyNever(
      () => authentication.authenticate(
        localizedReason: any(named: 'localizedReason'),
        authMessages: any(named: 'authMessages'),
        biometricOnly: any(named: 'biometricOnly'),
        sensitiveTransaction: any(named: 'sensitiveTransaction'),
        persistAcrossBackgrounding: any(named: 'persistAcrossBackgrounding'),
      ),
    );
  });

  test(
    'biometric protection does not fall back to a device passcode',
    () async {
      final result = await LocalSensitiveActionAuthorizer(
        authentication,
      ).authorizeBiometricProtection();

      expect(result, DeviceAuthResult.success);
      verify(
        () => authentication.authenticate(
          localizedReason: 'Enable biometrics for secure actions',
          authMessages: any(named: 'authMessages'),
          biometricOnly: true,
          sensitiveTransaction: true,
          persistAcrossBackgrounding: true,
        ),
      ).called(1);
    },
  );
}
