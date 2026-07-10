package com.promtuz.chat.ui.screens

import android.app.Activity
import android.content.Context
import android.hardware.biometrics.BiometricManager.Authenticators.BIOMETRIC_STRONG
import android.hardware.biometrics.BiometricManager.Authenticators.DEVICE_CREDENTIAL
import android.hardware.biometrics.BiometricPrompt
import android.os.CancellationSignal
import android.view.WindowManager
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import com.promtuz.chat.ui.components.FlexibleScreen
import com.promtuz.core.CoreBridge
import kotlinx.coroutines.launch

/**
 * Device-auth-gated reveal of the 24-word phrase (IDENTITY_RECOVERY.md §8:
 * the gate is MANDATORY — the phrase IS the private key, in words).
 * minSdk 31 ⇒ the framework BiometricPrompt with device-credential fallback,
 * no androidx.biometric dependency. FLAG_SECURE for the screen's lifetime.
 */
@Composable
fun RecoveryPhraseScreen() {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    val colors = MaterialTheme.colorScheme
    val typography = MaterialTheme.typography

    var words by remember { mutableStateOf<List<String>?>(null) }
    var denied by remember { mutableStateOf<String?>(null) }

    val window = (context as? Activity)?.window
    DisposableEffect(Unit) {
        window?.addFlags(WindowManager.LayoutParams.FLAG_SECURE)
        onDispose { window?.clearFlags(WindowManager.LayoutParams.FLAG_SECURE) }
    }

    fun unlock() {
        denied = null
        authenticate(
            context,
            onSuccess = {
                scope.launch {
                    try {
                        words = CoreBridge.exportRecoveryPhrase()
                    } catch (e: Exception) {
                        denied = e.message ?: "Could not load phrase"
                    }
                }
            },
            onError = { denied = it },
        )
    }

    LaunchedEffect(Unit) { unlock() }

    FlexibleScreen(
        { Text("Recovery Phrase") },
    ) { padding, _ ->
        Column(
            Modifier
                .fillMaxSize()
                .verticalScroll(rememberScrollState())
                .padding(padding)
                .padding(horizontal = 24.dp, vertical = 16.dp)
        ) {
            when (val w = words) {
                null -> {
                    Spacer(Modifier.height(48.dp))
                    Text(
                        denied ?: "Unlock to reveal your recovery phrase.",
                        style = typography.bodyLarge,
                        color = if (denied != null) colors.error else colors.onSurfaceVariant,
                    )
                    Spacer(Modifier.height(16.dp))
                    Button({ unlock() }) { Text("Unlock") }
                }

                else -> {
                    Text(
                        "These 24 words ARE your identity. Anyone who has them is you — " +
                            "never share them, never type them anywhere but this app.",
                        style = typography.bodyMedium,
                        color = colors.error,
                    )
                    Spacer(Modifier.height(20.dp))
                    Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
                        w.chunked(2).forEachIndexed { row, pair ->
                            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                                pair.forEachIndexed { col, word ->
                                    Box(
                                        Modifier
                                            .weight(1f)
                                            .clip(RoundedCornerShape(8.dp))
                                            .background(colors.surfaceContainerLow)
                                            .padding(vertical = 8.dp, horizontal = 12.dp),
                                        contentAlignment = Alignment.CenterStart,
                                    ) {
                                        Text(
                                            "${row * 2 + col + 1}. $word",
                                            style = typography.bodyLarge,
                                            fontFamily = FontFamily.Monospace,
                                        )
                                    }
                                }
                            }
                        }
                    }
                    Spacer(Modifier.height(20.dp))
                    Text(
                        "Written down? Anyone with these words can restore your identity " +
                            "on any device — treat the paper like a house key.",
                        style = typography.bodySmall,
                        color = colors.onSurfaceVariant,
                    )
                }
            }
        }
    }
}

/** Framework BiometricPrompt: strong biometric OR device credential. */
private fun authenticate(context: Context, onSuccess: () -> Unit, onError: (String) -> Unit) {
    val prompt = BiometricPrompt.Builder(context)
        .setTitle("Recovery phrase")
        .setDescription("Verify it's you before revealing the phrase")
        .setAllowedAuthenticators(BIOMETRIC_STRONG or DEVICE_CREDENTIAL)
        .build()
    prompt.authenticate(
        CancellationSignal(),
        context.mainExecutor,
        object : BiometricPrompt.AuthenticationCallback() {
            override fun onAuthenticationSucceeded(result: BiometricPrompt.AuthenticationResult?) {
                onSuccess()
            }

            override fun onAuthenticationError(errorCode: Int, errString: CharSequence?) {
                onError(errString?.toString() ?: "Authentication failed")
            }
        },
    )
}
