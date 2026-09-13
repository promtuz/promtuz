package com.promtuz.chat.security

import android.app.Activity
import android.app.KeyguardManager
import android.content.Context
import android.content.Intent
import android.hardware.biometrics.BiometricManager
import android.hardware.biometrics.BiometricManager.Authenticators.BIOMETRIC_STRONG
import android.hardware.biometrics.BiometricManager.Authenticators.DEVICE_CREDENTIAL
import android.hardware.biometrics.BiometricPrompt
import android.os.Build
import android.os.CancellationSignal
import androidx.annotation.RequiresApi
import com.promtuz.chat.R

internal fun Context.hasRecoveryScreenLock(): Boolean =
    getSystemService(KeyguardManager::class.java)?.isDeviceSecure == true

/** API 26–29 use the system credential Activity; API 30+ support the combined prompt. */
@Suppress("DEPRECATION")
internal fun Context.recoveryCredentialIntent(): Intent? =
    getSystemService(KeyguardManager::class.java)?.createConfirmDeviceCredentialIntent(
        getString(R.string.recovery_auth_title), getString(R.string.recovery_auth_description),
    )

internal fun Context.canUseRecoveryPrompt(credentialsOnly: Boolean): Boolean = Build.VERSION.SDK_INT >= Build.VERSION_CODES.R &&
    getSystemService(BiometricManager::class.java)
        ?.canAuthenticate(if (credentialsOnly) DEVICE_CREDENTIAL else BIOMETRIC_STRONG or DEVICE_CREDENTIAL) == BiometricManager.BIOMETRIC_SUCCESS

@RequiresApi(Build.VERSION_CODES.R)
internal fun Activity.authenticateRecoveryPhrase(
    cancellation: CancellationSignal,
    credentialsOnly: Boolean,
    onSuccess: () -> Unit,
    onError: (RecoveryNotice) -> Unit,
) {
    BiometricPrompt.Builder(this)
        .setTitle(getString(R.string.recovery_auth_title))
        .setDescription(getString(R.string.recovery_auth_description))
        .setAllowedAuthenticators(if (credentialsOnly) DEVICE_CREDENTIAL else BIOMETRIC_STRONG or DEVICE_CREDENTIAL)
        .build()
        .authenticate(cancellation, mainExecutor, object : BiometricPrompt.AuthenticationCallback() {
            override fun onAuthenticationSucceeded(result: BiometricPrompt.AuthenticationResult?) = onSuccess()

            override fun onAuthenticationError(errorCode: Int, errString: CharSequence?) {
                onError(when (errorCode) {
                    BiometricPrompt.BIOMETRIC_ERROR_CANCELED,
                    BiometricPrompt.BIOMETRIC_ERROR_USER_CANCELED -> RecoveryNotice.Cancelled
                    else -> RecoveryNotice.AuthenticationFailed
                })
            }
        })
}
