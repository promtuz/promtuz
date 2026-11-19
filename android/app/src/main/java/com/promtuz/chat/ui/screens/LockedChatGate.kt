package com.promtuz.chat.ui.screens

import com.promtuz.chat.ui.components.TextButton
import android.content.Context
import androidx.compose.animation.*
import androidx.compose.animation.core.EaseOutQuint
import androidx.compose.animation.core.tween
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.focusable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.hapticfeedback.HapticFeedbackType
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalHapticFeedback
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.unit.dp
import com.promtuz.chat.R
import com.promtuz.chat.presentation.viewmodel.AppVM
import com.promtuz.chat.security.PrivateChatSession
import com.promtuz.chat.ui.theme.Promtuz
import com.promtuz.chat.ui.components.*
import com.promtuz.chat.utils.extensions.fromHex
import com.promtuz.chat.utils.media.rememberAvatar
import com.promtuz.core.CoreBridge
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/** Stands in for a locked chat's content until its code opens a session; [onOpened] gets the session. */
@Composable
fun LockedChatGate(app: AppVM, conversation: String, name: String, onOpened: (Long) -> Unit) {
    DisposableEffect(Unit) {
        val protection = PrivateChatSession.protectInput()
        onDispose { protection.close() }
    }
    val context = LocalContext.current
    val haptic = LocalHapticFeedback.current
    val scope = rememberCoroutineScope()
    val pin = remember(conversation) { CoreBridge.privateCodeIsPin(conversation.fromHex()) }
    val chats by app.chats.collectAsState()
    val private by app.privateSummary.collectAsState()
    val chat = chats.firstOrNull { it.conversationHex == conversation } ?: private?.takeIf { it.conversationHex == conversation }

    var code by remember { mutableStateOf("") }
    var checking by remember { mutableStateOf(false) }
    var opened by remember { mutableStateOf(false) }
    var wrong by remember { mutableStateOf(false) }
    var shakes by remember { mutableIntStateOf(0) }
    var forgot by remember { mutableStateOf(false) }
    var resetting by remember { mutableStateOf(false) }
    var resetError by remember { mutableStateOf<String?>(null) }
    var retryAt by remember { mutableLongStateOf(Throttle.retryAt(context)) }
    var now by remember { mutableLongStateOf(System.currentTimeMillis()) }
    LaunchedEffect(retryAt) {
        while (retryAt > System.currentTimeMillis()) { now = System.currentTimeMillis(); delay(250) }
        now = System.currentTimeMillis()
    }
    val waitSeconds = ((retryAt - now + 999) / 1000).toInt().coerceAtLeast(0)
    val throttled = waitSeconds > 0
    val idle = !checking && !opened && !throttled

    fun open() {
        if (!idle || code.length < PIN_LENGTH) return
        checking = true
        val attempt = code
        scope.launch {
            val epoch = PrivateChatSession.generation
            val id = runCatching { CoreBridge.unlockPrivateConversation(attempt, epoch) }.getOrNull()
            // Another chat's code is a valid unlock too, but it must not open this one.
            if (id != null && CoreBridge.isPrivateConversationUnlocked(conversation.fromHex()) &&
                PrivateChatSession.arm(conversation)) {
                Throttle.reset(context)
                haptic.performHapticFeedback(HapticFeedbackType.Confirm)
                checking = false
                opened = true
                delay(180)
                PrivateChatSession.sessionId?.let(onOpened)
            } else {
                // Another chat's code opens nothing from here.
                if (id != null) PrivateChatSession.clear()
                haptic.performHapticFeedback(HapticFeedbackType.Reject)
                code = ""
                checking = false
                wrong = true
                retryAt = Throttle.fail(context)
                delay(150)
                shakes++
            }
        }
    }
    fun type(c: Char) {
        if (!idle || code.length >= PIN_LENGTH) return
        wrong = false
        code += c
        if (code.length == PIN_LENGTH) open()
    }
    fun erase() { if (idle) { code = code.dropLast(1); wrong = false } }

    val keys = remember { FocusRequester() }
    LaunchedEffect(Unit) { keys.requestFocus() }

    // Not a Surface, so text needs its color provided.
    CompositionLocalProvider(LocalContentColor provides MaterialTheme.colorScheme.onSurface) {
    Box(
        Modifier.fillMaxSize().background(MaterialTheme.colorScheme.background)
            .focusRequester(keys).focusable()
            .then(if (pin) Modifier.codeKeys(::type, ::erase) else Modifier),
    ) {
        Column(Modifier.fillMaxSize().systemBarsPadding().imePadding(), horizontalAlignment = Alignment.CenterHorizontally) {
            Row(Modifier.fillMaxWidth().padding(4.dp)) { GoBackButton() }
            Spacer(Modifier.weight(1f))

            LockedAvatar(name, chat, conversation, 80.dp, opened)
            Spacer(Modifier.height(16.dp))
            Text(name, Modifier.padding(horizontal = 32.dp), style = MaterialTheme.typography.titleLarge,
                maxLines = 1, overflow = TextOverflow.Ellipsis)
            Spacer(Modifier.height(4.dp))
            val hint = when {
                throttled && !pin -> "Too many tries. Try again in $waitSeconds ${if (waitSeconds == 1) "second" else "seconds"}"
                wrong -> "Wrong code"
                pin -> "Enter code"
                else -> "Enter password"
            }
            Text(hint, style = MaterialTheme.typography.bodyMedium,
                color = if (wrong && !throttled) MaterialTheme.colorScheme.error else MaterialTheme.colorScheme.onSurfaceVariant)
            Spacer(Modifier.height(24.dp))

            if (pin) {
                CodeDots(code, checking, Modifier.shake(shakes))
                Spacer(Modifier.weight(1f))
                Box(Modifier.height(codeKeypadHeight()).fillMaxWidth(), contentAlignment = Alignment.Center) {
                    androidx.compose.animation.AnimatedVisibility(!throttled,
                        enter = fadeIn(tween(320, easing = EaseOutQuint)) + scaleIn(tween(320, easing = EaseOutQuint), 0.8f),
                        exit = fadeOut(tween(320, easing = EaseOutQuint)) + scaleOut(tween(320, easing = EaseOutQuint), 0.8f)) {
                        CodeKeypad(::type, ::erase, { if (idle) code = "" })
                    }
                    androidx.compose.animation.AnimatedVisibility(throttled, enter = fadeIn(tween(320)), exit = fadeOut(tween(320))) {
                        Text("Too many tries.\nTry again in $waitSeconds ${if (waitSeconds == 1) "second" else "seconds"}",
                            textAlign = TextAlign.Center, style = MaterialTheme.typography.bodyLarge,
                            color = MaterialTheme.colorScheme.onSurfaceVariant)
                    }
                }
            } else {
                PasswordField(code, { if (idle) { code = it; wrong = false } }, enabled = idle,
                    onDone = ::open, modifier = Modifier.shake(shakes))
                Spacer(Modifier.weight(1f))
            }

            TextButton(enabled = !checking && !opened, onClick = { resetError = null; forgot = true },
                modifier = Modifier.padding(vertical = 8.dp)) {
                Text("Forgot code?", color = MaterialTheme.colorScheme.onSurfaceVariant)
            }
        }
    }
    }

    // Hidden chats and groups lose different things, so the dialog says which.
    val option by produceState<uniffi.core.PrivateResetOption?>(null, forgot) {
        value = if (!forgot) null else withContext(Dispatchers.IO) {
            runCatching { uniffi.core.lockedConversationResetOptions() }.getOrNull()
                ?.firstOrNull { it.conversationId.contentEquals(conversation.fromHex()) }
        }
    }
    if (forgot) AppAlertDialog(
        onDismissRequest = { if (!resetting) forgot = false },
        title = { Text(option?.resetTitle() ?: "Reset $name?") },
        text = { Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
            option?.let { Text(it.resetConsequence()) }
            resetError?.let { Text(it, color = MaterialTheme.colorScheme.error) }
        } },
        confirmButton = { TextButton(enabled = !resetting && option != null, onClick = {
            resetting = true
            scope.launch {
                // Once reset, the chat is no longer locked and its entry opens it.
                runCatching { CoreBridge.resetPrivateConversation(conversation.fromHex()) }
                    .onSuccess { forgot = false; PrivateChatSession.clear(); app.clearPrivateSummary() }
                    .onFailure { resetError = "Couldn't reset this chat" }
                resetting = false
            }
        }) { Text(if (option?.tag != null) "Delete chat" else "Delete history", color = MaterialTheme.colorScheme.error) } },
        dismissButton = { TextButton(enabled = !resetting, onClick = { forgot = false }) { Text("Cancel") } },
    )
}

/** Borderless centered password entry over a hairline, with a round check button, after Telegram's. */
@Composable
fun PasswordField(value: String, onChange: (String) -> Unit, enabled: Boolean, onDone: () -> Unit, modifier: Modifier = Modifier) {
    val focus = remember { FocusRequester() }
    LaunchedEffect(Unit) { focus.requestFocus() }
    val colors = MaterialTheme.colorScheme
    val ready = enabled && value.length >= PIN_LENGTH
    Box(modifier.fillMaxWidth().padding(horizontal = 24.dp), contentAlignment = Alignment.Center) {
        Column(Modifier.padding(horizontal = 56.dp), horizontalAlignment = Alignment.CenterHorizontally) {
            BasicTextField(
                value, onChange,
                modifier = Modifier.fillMaxWidth().focusRequester(focus).padding(vertical = 8.dp),
                // Never read-only: that drops the keyboard during a check or a wait. The caller ignores input instead.
                singleLine = true,
                textStyle = MaterialTheme.typography.headlineSmall.copy(color = colors.onSurface, textAlign = TextAlign.Center),
                cursorBrush = SolidColor(colors.primary),
                visualTransformation = PasswordVisualTransformation(),
                keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Password, autoCorrectEnabled = false, imeAction = ImeAction.Done),
                keyboardActions = KeyboardActions(onDone = { onDone() }),
            )
            Box(Modifier.fillMaxWidth().height(1.dp).background(colors.outlineVariant))
        }
        Box(
            Modifier.align(Alignment.CenterEnd).size(48.dp).clip(CircleShape)
                .background(if (ready) colors.primary else colors.surfaceContainerHigh)
                .clickable(enabled = ready, onClickLabel = "Done", onClick = onDone),
            contentAlignment = Alignment.Center,
        ) {
            DrawableIcon(R.drawable.oi_check, desc = "Done", size = 24.dp,
                tint = if (ready) colors.onPrimary else colors.onSurfaceVariant, modifier = Modifier.alpha(if (ready) 1f else 0.6f))
        }
    }
}

/** Telegram's schedule after three wrong codes: 5 s, then 5 s more per try, up to 30 s. */
private object Throttle {
    // ponytail: wall clock, so changing the system time skips the wait; core-side counting if it matters.
    private fun prefs(c: Context) = c.getSharedPreferences("private_lock", Context.MODE_PRIVATE)
    fun retryAt(c: Context) = prefs(c).getLong("retry_at", 0L)
    fun fail(c: Context): Long {
        val p = prefs(c)
        val tries = p.getInt("tries", 0) + 1
        val wait = if (tries < 3) 0 else minOf(30, (tries - 2) * 5)
        val at = if (wait > 0) System.currentTimeMillis() + wait * 1000L else 0L
        p.edit().putInt("tries", tries).putLong("retry_at", at).apply()
        return at
    }
    fun reset(c: Context) = prefs(c).edit().clear().apply()
}

/** The chat's avatar with a lock badge; the gate and lock setup share it. */
@Composable
internal fun LockedAvatar(name: String, chat: com.promtuz.chat.domain.model.ChatSummary?, conversation: String, size: androidx.compose.ui.unit.Dp, opened: Boolean = false) {
    val c = Promtuz.colors
    Box(Modifier.size(size)) {
        if (chat?.isGroup == true) GroupAvatar(name, emptyList(), size, conversation = conversation)
        else Avatar(name, size, identityKey = chat?.peerHex ?: conversation, image = rememberAvatar(chat?.peerHex))
        AvatarBadge(size, Modifier.align(Alignment.BottomEnd)) {
            Crossfade(opened, animationSpec = tween(180), label = "lock") { open ->
                DrawableIcon(if (open) R.drawable.oi_lock_chat_open else R.drawable.oi_lock_chat, size = size * 0.2f, tint = c.muted)
            }
        }
    }
}
