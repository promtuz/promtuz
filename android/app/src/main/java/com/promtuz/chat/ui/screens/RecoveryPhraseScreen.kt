package com.promtuz.chat.ui.screens

import com.promtuz.chat.ui.theme.Radii
import androidx.compose.runtime.getValue
import androidx.compose.runtime.setValue
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.height
import androidx.compose.ui.draw.blur
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.sp
import com.promtuz.chat.navigation.Routes
import com.promtuz.chat.presentation.viewmodel.AppVM
import com.promtuz.chat.ui.components.*
import com.promtuz.chat.ui.theme.Promtuz
import org.koin.compose.koinInject
import com.promtuz.chat.ui.components.TextButton
import com.promtuz.chat.ui.components.IconButton
import com.promtuz.chat.ui.components.AppButton
import com.promtuz.chat.ui.components.ButtonKind
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
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
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

    val app = koinInject<AppVM>()
    RecoveryPhraseContent(state, hasScreenLock, settingsError,
        onBackup = { app.navigator.push(Routes.BackupRestore) },
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
    onBackup: () -> Unit,
    onReveal: () -> Unit,
    onUseCredentials: () -> Unit,
    onHide: () -> Unit,
    onOpenSettings: () -> Unit,
) {
    val c = Promtuz.colors
    val revealed = state as? RecoveryPhraseState.Revealed
    val busy = revealed == null && state !is RecoveryPhraseState.Locked
    val error = (state as? RecoveryPhraseState.Locked)?.notice
        ?.takeIf { it != RecoveryNotice.Cancelled && it != RecoveryNotice.Hidden }
    GroupedScreen(
        { Text(stringResource(R.string.recovery_title), maxLines = 1, overflow = TextOverflow.Ellipsis) },
        connectionStatus = false,
        actions = {
            if (revealed != null) IconButton(onHide, Modifier.testTag("recovery-hide-top")) {
                Icon(painterResource(R.drawable.oi_eye_slash), stringResource(R.string.recovery_hide))
            }
        },
    ) { padding ->
        LazyColumn(
            Modifier.fillMaxSize(),
            contentPadding = padding.listPadding(top = 6.dp, bottom = 40.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            item("phrase") {
                Column(
                    Modifier.fillMaxWidth().clip(RoundedCornerShape(Radii.card)).background(cardFill())
                        .padding(start = 12.dp, end = 12.dp, top = 12.dp, bottom = 16.dp),
                    verticalArrangement = Arrangement.spacedBy(16.dp),
                ) {
                    Box {
                        RecoveryWordGrid(revealed?.words)
                        if (revealed == null) Column(
                            Modifier.matchParentSize().clip(RoundedCornerShape(Radii.field)).background(c.card.copy(alpha = 0.2f)),
                            horizontalAlignment = Alignment.CenterHorizontally,
                            verticalArrangement = Arrangement.spacedBy(8.dp, Alignment.CenterVertically),
                        ) {
                            DrawableIcon(R.drawable.oi_eye_slash, tint = c.text, size = 26.dp)
                            Text(
                                stringResource(if (hasScreenLock) R.string.recovery_locked_title else R.string.recovery_no_lock_title),
                                style = TextStyle(fontSize = 14.sp, lineHeight = 20.sp, fontWeight = FontWeight.Medium),
                                color = c.text,
                            )
                        }
                    }
                    when {
                        revealed != null -> AppButton(stringResource(R.string.recovery_hide), onHide,
                            Modifier.fillMaxWidth().testTag("recovery-hide-bottom"), kind = ButtonKind.Tonal)
                        !hasScreenLock -> AppButton(stringResource(R.string.recovery_open_settings), onOpenSettings, Modifier.fillMaxWidth())
                        else -> AppButton(stringResource(R.string.recovery_reveal), onReveal, Modifier.fillMaxWidth(), busy = busy)
                    }
                    val note = when {
                        settingsError -> stringResource(R.string.recovery_settings_unavailable)
                        error != null -> stringResource(error.messageResource())
                        revealed != null -> stringResource(R.string.recovery_auto_hide)
                        else -> null
                    }
                    note?.let {
                        Text(it, Modifier.fillMaxWidth().semantics { liveRegion = LiveRegionMode.Polite },
                            style = FootnoteStyle, color = if (settingsError || error != null) c.danger else c.muted,
                            textAlign = TextAlign.Center)
                    }
                    if (revealed == null && hasScreenLock && !busy && Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
                        TextButton(onUseCredentials, Modifier.align(Alignment.CenterHorizontally)) {
                            Text(stringResource(R.string.recovery_use_screen_lock))
                        }
                    }
                }
            }
            item("note") {
                Text(stringResource(R.string.recovery_intro_body), Modifier.padding(horizontal = 16.dp),
                    style = FootnoteStyle, color = c.muted)
            }
            item("backup") {
                SettingsCard {
                    SettingsRow("Chat backup", icon = R.drawable.i_chat_backup, trailing = Trailing.Chevron, onClick = onBackup)
                }
            }
        }
    }
}

/** Without [words] it draws blurred stand-ins, so the hidden state never holds the real phrase. */
@Composable
private fun RecoveryWordGrid(words: List<String>?) {
    val c = Promtuz.colors
    val fontScale = LocalDensity.current.fontScale
    BoxWithConstraints(Modifier.fillMaxWidth()) {
        val columns = if (maxWidth / fontScale < 300.dp) 2 else 3
        val shown = words ?: StandIns
        Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
            shown.chunked(columns).forEachIndexed { rowIndex, row ->
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    row.forEachIndexed { columnIndex, word ->
                        val number = rowIndex * columns + columnIndex + 1
                        val description = stringResource(R.string.recovery_word_description, number, word)
                        Row(
                            Modifier.weight(1f).height(40.dp).clip(RoundedCornerShape(Radii.small)).background(c.cardHigh)
                                .then(if (words == null) Modifier.clearAndSetSemantics { } else Modifier.clearAndSetSemantics { contentDescription = description })
                                .padding(horizontal = 12.dp),
                            verticalAlignment = Alignment.CenterVertically,
                            horizontalArrangement = Arrangement.spacedBy(8.dp),
                        ) {
                            Text(number.toString(), style = TextStyle(fontSize = 12.sp, lineHeight = 16.sp), color = c.muted)
                            // Blur needs API 31; older devices get a plain bar of the same width.
                            if (words == null && Build.VERSION.SDK_INT < Build.VERSION_CODES.S) Box(Modifier.width((word.length * 8).dp)
                                .height(10.dp).clip(RoundedCornerShape(5.dp)).background(c.muted.copy(alpha = 0.3f)))
                            else Text(word, Modifier.then(if (words == null) Modifier.blur(7.dp) else Modifier),
                                style = TextStyle(fontSize = 15.sp, lineHeight = 20.sp), color = c.text, maxLines = 1)
                        }
                    }
                    repeat(columns - row.size) { Spacer(Modifier.weight(1f)) }
                }
            }
        }
    }
}

private val StandIns = listOf(
    "orbit", "maple", "candle", "river", "quartz", "lemon", "harbor", "velvet", "tiger", "pencil", "meadow", "copper",
    "falcon", "nectar", "summit", "willow", "ember", "garden", "rocket", "violet", "anchor", "pebble", "breeze", "island",
)

private fun RecoveryNotice.messageResource(): Int = when (this) {
    RecoveryNotice.Cancelled -> R.string.recovery_cancelled
    RecoveryNotice.Hidden -> R.string.recovery_hidden
    RecoveryNotice.AuthenticationFailed -> R.string.recovery_auth_failed
    RecoveryNotice.LoadFailed -> R.string.recovery_load_failed
    RecoveryNotice.Unavailable -> R.string.recovery_unavailable
}
