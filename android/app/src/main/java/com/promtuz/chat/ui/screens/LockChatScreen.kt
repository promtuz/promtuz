package com.promtuz.chat.ui.screens

import com.promtuz.chat.ui.components.IconButton
import androidx.activity.compose.BackHandler
import androidx.compose.animation.*
import androidx.compose.animation.core.tween
import androidx.compose.foundation.focusable
import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.hapticfeedback.HapticFeedbackType
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalHapticFeedback
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import com.promtuz.chat.R
import com.promtuz.chat.navigation.LocalNavForeground
import com.promtuz.chat.security.PrivateChatSession
import com.promtuz.chat.ui.components.*
import com.promtuz.chat.utils.extensions.fromHex
import com.promtuz.core.CoreBridge
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import com.promtuz.chat.ui.theme.Promtuz
import com.promtuz.chat.ui.text.ttCommonsProFamily
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.sp

@Composable
fun LockChatScreen(conversation: String, onBack: () -> Unit, onLocked: () -> Unit) {
    DisposableEffect(Unit) {
        val protection = PrivateChatSession.protectInput()
        onDispose { protection.close() }
    }
    val context = LocalContext.current
    val haptic = LocalHapticFeedback.current
    val scope = rememberCoroutineScope()
    val foreground = LocalNavForeground.current
    var pin by remember { mutableStateOf(true) }
    var code by remember { mutableStateOf("") }
    var first by remember { mutableStateOf<String?>(null) }
    var busy by remember { mutableStateOf(false) }
    var done by remember { mutableStateOf(false) }
    var error by remember { mutableStateOf<String?>(null) }
    var errorShown by remember { mutableIntStateOf(0) }
    var shakes by remember { mutableIntStateOf(0) }
    LaunchedEffect(errorShown) { if (errorShown > 0) { delay(3000); error = null } }

    fun fail(message: String) {
        haptic.performHapticFeedback(HapticFeedbackType.Reject)
        code = ""
        error = message
        errorShown++
        shakes++
    }
    fun restart() { code = ""; first = null; error = null }
    BackHandler(busy || first != null) { if (!busy) restart() }

    fun submit() {
        if (!foreground || busy || code.length < PIN_LENGTH) return
        val chosen = code
        val confirmed = first ?: run { first = chosen; code = ""; error = null; return }
        if (chosen != confirmed) return fail("Codes don't match. Try again.")
        busy = true
        scope.launch {
            runCatching { CoreBridge.hideConversation(conversation.fromHex(), chosen) }
                .onSuccess {
                    com.promtuz.core.call.CallNotifications.clearConversation(context, conversation.fromHex())
                    runCatching { com.promtuz.core.push.PushNotifier.refresh() }
                    haptic.performHapticFeedback(HapticFeedbackType.Confirm)
                    done = true
                    delay(600)
                    onLocked()
                }
                .onFailure { first = null; fail(it.message ?: "Couldn't lock this chat") }
            busy = false
        }
    }
    fun type(c: Char) {
        if (busy || done || code.length >= PIN_LENGTH) return
        code += c
        if (code.length == PIN_LENGTH) submit()
    }
    fun erase() { if (!busy && !done) code = code.dropLast(1) }

    val app = org.koin.compose.koinInject<com.promtuz.chat.presentation.viewmodel.AppVM>()
    val chats by app.chats.collectAsState()
    val chat = chats.firstOrNull { it.conversationHex == conversation }
    val keys = remember { FocusRequester() }
    LaunchedEffect(pin) { if (pin) keys.requestFocus() }

    SimpleScreen(
        title = { Text("Lock chat") },
        connectionStatus = false,
        navigationIcon = {
            IconButton(onClick = { if (first != null) restart() else onBack() }, enabled = !busy) {
                TopBarMorphIcon(MorphGlyph.Back, "Back")
            }
        },
    ) { padding ->
        Column(
            Modifier.fillMaxSize().padding(top = padding.calculateTopPadding()).consumeWindowInsets(padding).imePadding()
                .focusRequester(keys).focusable().then(if (pin) Modifier.codeKeys(::type, ::erase) else Modifier),
            horizontalAlignment = Alignment.CenterHorizontally,
        ) {
            Spacer(Modifier.weight(1f))
            LockedAvatar(chat?.name.orEmpty(), chat, conversation, 72.dp)
            Spacer(Modifier.height(20.dp))
            AnimatedContent(first != null, transitionSpec = { fadeIn(tween(200)) togetherWith fadeOut(tween(200)) },
                label = "step") { confirming ->
                Column(Modifier.padding(horizontal = 32.dp), horizontalAlignment = Alignment.CenterHorizontally) {
                    Text(if (confirming) "Re-enter your code" else "Create a code", style = LockTitleStyle, color = Promtuz.colors.text)
                    Spacer(Modifier.height(8.dp))
                    Text(
                        if (confirming) "If you forget it, resetting deletes this chat's history."
                        else "You'll need it to open this chat.\nIt won't send notifications.",
                        style = LockBodyStyle, textAlign = TextAlign.Center, minLines = 2,
                        color = Promtuz.colors.muted,
                    )
                }
            }
            Spacer(Modifier.height(28.dp))
            if (pin) CodeBoxes(code, done, Modifier.shake(shakes))
            else PasswordField(code, { if (!busy && !done) code = it }, enabled = !busy && !done,
                onDone = ::submit, modifier = Modifier.shake(shakes))
            Box(Modifier.height(52.dp), contentAlignment = Alignment.Center) {
                Crossfade(error, animationSpec = tween(180), label = "hint") { message ->
                    if (message != null) Text(message, style = LockBodyStyle, color = Promtuz.colors.danger)
                    else if (first == null) TextButton(onClick = { if (!busy) { pin = !pin; restart() } }, enabled = !busy && !done) {
                        Text(if (pin) "Use a password instead" else "Use a code instead")
                    }
                }
            }
            Spacer(Modifier.weight(1f))
            if (pin) CodeKeypad(::type, ::erase, { if (!busy && !done) code = "" },
                Modifier.padding(bottom = padding.calculateBottomPadding() + 24.dp))
        }
    }
}

private val LockTitleStyle = TextStyle(fontFamily = ttCommonsProFamily, fontWeight = FontWeight.SemiBold, fontSize = 24.sp, lineHeight = 30.sp)
private val LockBodyStyle = TextStyle(fontSize = 15.sp, lineHeight = 20.sp)
