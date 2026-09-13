package com.promtuz.chat.ui.screens

import android.app.Activity
import android.content.Intent
import android.os.Build
import android.os.CancellationSignal
import android.provider.Settings
import androidx.activity.compose.LocalActivity
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.semantics.LiveRegionMode
import androidx.compose.ui.semantics.clearAndSetSemantics
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.liveRegion
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import androidx.lifecycle.compose.LocalLifecycleOwner
import com.promtuz.chat.R
import com.promtuz.chat.security.RecoveryNotice
import com.promtuz.chat.security.RecoveryPhraseSession
import com.promtuz.chat.security.RecoveryPhraseState
import com.promtuz.chat.security.SecureRecoveryWindow
import com.promtuz.chat.security.authenticateRecoveryPhrase
import com.promtuz.chat.security.canUseRecoveryPrompt
import com.promtuz.chat.security.hasRecoveryScreenLock
import com.promtuz.chat.security.recoveryCredentialIntent
import com.promtuz.chat.ui.components.SimpleScreen
import com.promtuz.core.CoreBridge
import com.promtuz.chat.navigation.LocalNavCardExiting

/** Secrets and authorization live only for this composition, never in saved state. */
@Composable
fun RecoveryPhraseScreen() {
    val context = LocalContext.current
    val activity = LocalActivity.current
    val lifecycle = LocalLifecycleOwner.current.lifecycle
    val scope = rememberCoroutineScope()
    val session = remember(scope, lifecycle) { RecoveryPhraseSession(scope, CoreBridge::exportRecoveryPhrase) }
    val state by session.state.collectAsState()
    var hasScreenLock by remember { mutableStateOf(context.hasRecoveryScreenLock()) }
    var settingsError by remember { mutableStateOf(false) }
    var cancellation by remember { mutableStateOf<CancellationSignal?>(null) }
    var credentialAttempt by remember { mutableStateOf<Int?>(null) }

    val credentials = rememberLauncherForActivityResult(ActivityResultContracts.StartActivityForResult()) { result ->
        val attempt = credentialAttempt
        credentialAttempt = null
        if (attempt != null) {
            if (result.resultCode == Activity.RESULT_OK) session.authenticated(attempt)
            else session.authenticationFailed(attempt, RecoveryNotice.Cancelled)
        }
    }

    val window = activity?.window
    DisposableEffect(window) {
        window?.let(SecureRecoveryWindow::acquire)
        onDispose {
            window?.let(SecureRecoveryWindow::release)
        }
    }
    DisposableEffect(session, lifecycle) {
        fun refresh() {
            session.setForeground(lifecycle.currentState.isAtLeast(Lifecycle.State.RESUMED))
            hasScreenLock = context.hasRecoveryScreenLock()
        }
        val observer = LifecycleEventObserver { _, event ->
            when (event) {
                Lifecycle.Event.ON_RESUME -> refresh()
                Lifecycle.Event.ON_PAUSE -> session.setForeground(false)
                else -> Unit
            }
        }
        lifecycle.addObserver(observer)
        refresh()
        onDispose {
            lifecycle.removeObserver(observer)
            session.close()
            cancellation?.cancel()
        }
    }
    val exiting = LocalNavCardExiting.current
    LaunchedEffect(exiting) {
        if (exiting) {
            session.lock()
            cancellation?.cancel()
            credentialAttempt = null
        }
    }

    fun reveal(credentialsOnly: Boolean) {
        hasScreenLock = context.hasRecoveryScreenLock()
        if (!hasScreenLock) return
        val attempt = session.beginAuthentication() ?: return
        if (window == null) {
            session.authenticationFailed(attempt, RecoveryNotice.Unavailable)
            return
        }
        try {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R &&
                context.canUseRecoveryPrompt(credentialsOnly)) {
                val signal = CancellationSignal()
                cancellation = signal
                activity.authenticateRecoveryPhrase(signal, credentialsOnly,
                    onSuccess = { session.authenticated(attempt) },
                    onError = { session.authenticationFailed(attempt, it) })
            } else {
                val intent = context.recoveryCredentialIntent()
                if (intent == null) session.authenticationFailed(attempt, RecoveryNotice.Unavailable)
                else {
                    credentialAttempt = attempt
                    credentials.launch(intent)
                }
            }
        } catch (_: Exception) {
            credentialAttempt = null
            session.authenticationFailed(attempt, RecoveryNotice.Unavailable)
        }
    }

    RecoveryPhraseContent(state, hasScreenLock, settingsError,
        onReveal = { reveal(false) }, onUseCredentials = { reveal(true) },
        onHide = {
            session.lock(if (state is RecoveryPhraseState.Revealed) RecoveryNotice.Hidden else RecoveryNotice.Cancelled)
            cancellation?.cancel()
            credentialAttempt = null
        },
        onOpenSettings = {
            settingsError = false
            try { context.startActivity(Intent(Settings.ACTION_SECURITY_SETTINGS)) }
            catch (_: Exception) { settingsError = true }
        })
}

@Composable
internal fun RecoveryPhraseContent(
    state: RecoveryPhraseState,
    hasScreenLock: Boolean,
    settingsError: Boolean,
    onReveal: () -> Unit,
    onUseCredentials: () -> Unit,
    onHide: () -> Unit,
    onOpenSettings: () -> Unit,
) {
    val colors = MaterialTheme.colorScheme
    val typography = MaterialTheme.typography
    SimpleScreen(
        { Text(stringResource(R.string.recovery_title), maxLines = 1, overflow = TextOverflow.Ellipsis) },
        actions = {
            if (state is RecoveryPhraseState.Revealed) {
                IconButton(onHide, Modifier.testTag("recovery-hide-top")) {
                    Icon(painterResource(R.drawable.i_shield_lock), stringResource(R.string.recovery_hide))
                }
            }
        },
    ) { padding ->
        Column(
            Modifier.fillMaxSize().padding(padding).verticalScroll(rememberScrollState())
                .padding(start = 18.dp, end = 18.dp, top = 12.dp, bottom = 32.dp),
            verticalArrangement = Arrangement.spacedBy(20.dp),
        ) {
            Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                Text(stringResource(R.string.recovery_intro_body), style = typography.bodyMedium, color = colors.onSurfaceVariant)
            }
            Text(stringResource(R.string.recovery_private_body), style = typography.bodyMedium,
                color = colors.onSurface)
            if (state is RecoveryPhraseState.Revealed) {
                Column {
                    Text(stringResource(R.string.recovery_words_label), style = typography.titleSmall)
                    Text(stringResource(R.string.recovery_words_instruction), style = typography.bodySmall, color = colors.onSurfaceVariant)
                }
                RecoveryWordGrid(state.words)
                Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
                    Text(stringResource(R.string.recovery_auto_hide), style = typography.bodySmall, color = colors.onSurfaceVariant)
                    TextButton(onHide, Modifier.align(Alignment.End).testTag("recovery-hide-bottom")) {
                        Text(stringResource(R.string.recovery_hide))
                    }
                }
            } else {
                val busy = state !is RecoveryPhraseState.Locked
                Column(Modifier.fillMaxWidth(), verticalArrangement = Arrangement.spacedBy(12.dp)) {
                        Text(stringResource(if (hasScreenLock) R.string.recovery_locked_title else R.string.recovery_no_lock_title),
                            style = typography.titleMedium)
                        Text(stringResource(if (hasScreenLock) R.string.recovery_locked_body else R.string.recovery_no_lock_body),
                            style = typography.bodyMedium, color = colors.onSurfaceVariant)
                        if (state is RecoveryPhraseState.Locked && state.notice != null) {
                            val isError = state.notice !in listOf(RecoveryNotice.Cancelled, RecoveryNotice.Hidden)
                            Text(stringResource(state.notice.messageResource()),
                                Modifier.semantics { liveRegion = LiveRegionMode.Polite },
                                style = typography.bodyMedium, color = if (isError) colors.error else colors.onSurfaceVariant)
                        }
                        if (settingsError) Text(stringResource(R.string.recovery_settings_unavailable), color = colors.error)
                        if (!hasScreenLock) {
                            Button(onOpenSettings, Modifier.fillMaxWidth()) { Text(stringResource(R.string.recovery_open_settings)) }
                        } else if (busy) {
                            Row(Modifier.fillMaxWidth().semantics { liveRegion = LiveRegionMode.Polite },
                                verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                                CircularProgressIndicator(Modifier.size(20.dp), strokeWidth = 2.dp)
                                Text(stringResource(if (state == RecoveryPhraseState.Loading) R.string.recovery_loading else R.string.recovery_verifying),
                                    style = typography.bodyMedium)
                            }
                            TextButton(onHide, Modifier.fillMaxWidth()) { Text(stringResource(R.string.recovery_cancel)) }
                        } else {
                            Button(onReveal, Modifier.fillMaxWidth()) { Text(stringResource(R.string.recovery_reveal)) }
                            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
                                TextButton(onUseCredentials, Modifier.fillMaxWidth()) {
                                    Text(stringResource(R.string.recovery_use_screen_lock))
                                }
                            }
                        }
                }
            }
            Text(stringResource(R.string.identity_history_note), Modifier.padding(horizontal = 4.dp),
                style = typography.bodySmall, color = colors.onSurfaceVariant)
        }
    }
}

@Composable
private fun RecoveryWordGrid(words: List<String>) {
    val fontScale = LocalDensity.current.fontScale
    BoxWithConstraints(Modifier.fillMaxWidth()) {
        val columns = when {
            maxWidth / fontScale < 280.dp -> 1
            maxWidth / fontScale < 440.dp -> 2
            else -> 3
        }
        Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
            words.chunked(columns).forEachIndexed { rowIndex, row ->
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    row.forEachIndexed { columnIndex, word ->
                        val number = rowIndex * columns + columnIndex + 1
                        val description = stringResource(R.string.recovery_word_description, number, word)
                        Row(Modifier.weight(1f).clip(RoundedCornerShape(12.dp))
                            .background(MaterialTheme.colorScheme.surfaceContainerLow)
                            .clearAndSetSemantics { contentDescription = description }
                            .padding(horizontal = 12.dp, vertical = 14.dp),
                            verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(10.dp)) {
                            Text(number.toString(), style = MaterialTheme.typography.labelSmall,
                                color = MaterialTheme.colorScheme.onSurfaceVariant)
                            Text(word, style = MaterialTheme.typography.bodyLarge, fontFamily = FontFamily.Monospace)
                        }
                    }
                    repeat(columns - row.size) { Spacer(Modifier.weight(1f)) }
                }
            }
        }
    }
}

private fun RecoveryNotice.messageResource(): Int = when (this) {
    RecoveryNotice.Cancelled -> R.string.recovery_cancelled
    RecoveryNotice.Hidden -> R.string.recovery_hidden
    RecoveryNotice.AuthenticationFailed -> R.string.recovery_auth_failed
    RecoveryNotice.LoadFailed -> R.string.recovery_load_failed
    RecoveryNotice.Unavailable -> R.string.recovery_unavailable
}
