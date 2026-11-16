package com.promtuz.chat

import androidx.compose.foundation.layout.fillMaxSize

import android.content.Intent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.IntentFilter
import android.os.PowerManager
import androidx.lifecycle.lifecycleScope
import com.promtuz.chat.security.PrivateChatSession
import kotlinx.coroutines.launch
import kotlinx.coroutines.flow.collectLatest
import androidx.lifecycle.withResumed
import android.os.Bundle
import android.widget.Toast
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.Composable
import androidx.compose.ui.platform.LocalContext
import dev.chrisbanes.haze.hazeSource
import androidx.core.splashscreen.SplashScreen.Companion.installSplashScreen
import com.promtuz.chat.navigation.AppNavigation
import com.promtuz.chat.navigation.Routes
import com.promtuz.chat.ui.appearance.AppearanceStore
import com.promtuz.chat.presentation.viewmodel.AppVM
import com.promtuz.chat.ui.components.InviteBottomSheet
import com.promtuz.chat.ui.media.MediaViewerHost
import com.promtuz.chat.ui.screens.RequiredUpdateHost
import com.promtuz.chat.update.UpdateNotifier
import com.promtuz.chat.update.UpdateRepository
import com.promtuz.chat.ui.theme.PromtuzTheme
import com.promtuz.chat.utils.InviteLink
import com.promtuz.core.CoreBridge
import com.promtuz.core.adapter.CoreEventBus
import com.promtuz.core.push.PushNotifier
import org.koin.android.ext.android.inject
import uniffi.core.MediaAlert

class LauncherActivity : ComponentActivity() {
    private val viewModel: AppVM by inject()
    private val updates: UpdateRepository by inject()

    private val screenReceiver = object : BroadcastReceiver() {
        override fun onReceive(context: Context?, intent: Intent?) {
            if (intent?.action == Intent.ACTION_SCREEN_OFF) {
                PrivateChatSession.screenOff()
                enforcePrivateExit()
            }
        }
    }
    private var exitVersion = PrivateChatSession.exits.value

    private fun enforcePrivateExit() {
        if (exitVersion == PrivateChatSession.exits.value) return
        exitVersion = PrivateChatSession.exits.value
        // Cover synchronously before another frame or a task snapshot can render old content.
        window.decorView.visibility = android.view.View.INVISIBLE
        viewModel.clearPrivateSummary()
        com.promtuz.chat.ui.media.MediaViewer.close()
        com.promtuz.chat.utils.media.VoicePlayer.stop()
        com.promtuz.chat.utils.media.clearDecodedImageCache()
        viewModel.navigator.reset(Routes.App)
        finishAndRemoveTask()
    }

    override fun onResume() {
        super.onResume()
        com.promtuz.chat.security.ScreenSecurity.apply(window)
        PrivateChatSession.resumed(getSystemService(PowerManager::class.java).isInteractive)
        enforcePrivateExit()
    }

    override fun onPause() {
        PrivateChatSession.paused()
        enforcePrivateExit()
        super.onPause()
    }

    override fun onDestroy() {
        unregisterReceiver(screenReceiver)
        // Rotation must not keep a private capability alive through activity reconstruction.
        if (isChangingConfigurations) PrivateChatSession.lock()
        super.onDestroy()
    }

    override fun startActivityForResult(intent: Intent, requestCode: Int, options: Bundle?) {
        PrivateChatSession.beginHandoff()
        try { super.startActivityForResult(intent, requestCode, options) }
        catch (error: RuntimeException) { PrivateChatSession.clear(); throw error }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        installSplashScreen()
        super.onCreate(savedInstanceState)

        androidx.core.content.ContextCompat.registerReceiver(
            this, screenReceiver, IntentFilter(Intent.ACTION_SCREEN_OFF),
            androidx.core.content.ContextCompat.RECEIVER_NOT_EXPORTED,
        )
        lifecycleScope.launch {
            PrivateChatSession.exits.collect { enforcePrivateExit() }
        }
        lifecycleScope.launch {
            var held = false
            PrivateChatSession.secure.collectLatest { secure ->
                if (secure && !held) {
                    com.promtuz.chat.security.SecureRecoveryWindow.acquire(window)
                    held = true
                } else if (!secure && held) {
                    // Release only after the private screen's exit animation and only in front,
                    // so no task snapshot can catch its last frame.
                    kotlinx.coroutines.delay(1000)
                    lifecycle.withResumed {
                        com.promtuz.chat.security.SecureRecoveryWindow.release(window)
                        held = false
                    }
                }
            }
        }
        enableEdgeToEdge()
        consumeContactCard(intent)
        consumeInvite(intent)
        consumeChatOpen(intent)
        consumeUpdateOpen(intent)

        setContent {
            val appearance by AppearanceStore.appearance.collectAsState()
            PromtuzTheme(appearance = appearance) {
                val rootHaze = dev.chrisbanes.haze.rememberHazeState()
                androidx.compose.runtime.CompositionLocalProvider(com.promtuz.chat.ui.components.LocalRootHaze provides rootHaze) {
                    androidx.compose.foundation.layout.Box(
                        androidx.compose.ui.Modifier.fillMaxSize().hazeSource(rootHaze)
                    ) {
                        AppNavigation(viewModel)
                        MediaViewerHost()
                        com.promtuz.chat.ui.camera.CameraOverlayHost()
                        InviteBottomSheet(viewModel)
                        RequiredUpdateHost()
                        MediaAlertHost()
                    }
                }
            }
        }
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        consumeContactCard(intent)
        consumeInvite(intent)
        consumeChatOpen(intent)
        consumeUpdateOpen(intent)
    }

    private fun consumeContactCard(intent: Intent) {
        if (intent.getBooleanExtra("open_message_requests", false)) {
            intent.removeExtra("open_message_requests")
            if (CoreBridge.shouldLaunchApp()) viewModel.navigator.openExternal(Routes.MessageRequests)
        }
        val uri = intent.data ?: return
        if (!((uri.scheme == "https" && uri.host == "promtuz.dev" && uri.path == "/contact") ||
            (uri.scheme == "promtuz" && uri.host == "contact"))) return
        val encoded = uri.fragment?.takeIf { it.length <= 1024 && it.matches(Regex("[A-Za-z0-9_-]+")) } ?: return
        intent.data = null
        if (CoreBridge.shouldLaunchApp()) viewModel.navigator.openExternal(Routes.ContactCard(encoded = encoded))
        else viewModel.pendingContactCard = encoded
    }

    private fun consumeUpdateOpen(intent: Intent) {
        if (!intent.getBooleanExtra(UpdateNotifier.EXTRA_OPEN_UPDATE, false)) return
        intent.removeExtra(UpdateNotifier.EXTRA_OPEN_UPDATE)
        viewModel.navigator.openExternal(Routes.Updates)
        updates.check()
    }

    private fun consumeInvite(intent: Intent) {
        val invite = intent.getByteArrayExtra(InviteLink.EXTRA_INVITE)
            ?: intent.data?.let(InviteLink::decode)
            ?: return
        intent.removeExtra(InviteLink.EXTRA_INVITE) // not replayed on recreation
        if (CoreBridge.shouldLaunchApp()) viewModel.showInvite(invite) else viewModel.pendingInvite = invite
    }

    /** The activity is exported, so any app can send these extras: the id must have a real
     *  one's shape before it reaches a route that parses it. */
    private fun consumeChatOpen(intent: Intent) {
        val convHex = intent.getStringExtra(PushNotifier.EXTRA_CONVERSATION)
            ?.takeIf { it.matches(Regex("[0-9a-f]{32}")) } ?: return
        val name = intent.getStringExtra(PushNotifier.EXTRA_CONV_NAME).orEmpty()
        intent.removeExtra(PushNotifier.EXTRA_CONVERSATION) // not replayed on recreation
        intent.removeExtra(PushNotifier.EXTRA_CONV_NAME)
        if (CoreBridge.shouldLaunchApp()) viewModel.openChat(convHex, name)
    }
}

@Composable
private fun MediaAlertHost() {
    val context = LocalContext.current
    LaunchedEffect(Unit) {
        CoreEventBus.mediaAlerts.collect { alert ->
            val message = when (alert) {
                MediaAlert.ANIMATION_SIMPLIFIED -> "Sent as a still image"
                MediaAlert.HDR_CONVERTED_TO_SDR -> "HDR was converted to SDR"
                MediaAlert.VIDEO_AUDIO_REMOVED -> "Sent without audio"
            }
            Toast.makeText(context, message, Toast.LENGTH_LONG).show()
        }
    }
}
