package com.promtuz.chat

import androidx.compose.foundation.layout.fillMaxSize

import android.content.Intent
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
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
import com.promtuz.core.push.PushNotifier
import org.koin.android.ext.android.inject

class LauncherActivity : ComponentActivity() {
    private val viewModel: AppVM by inject()
    private val updates: UpdateRepository by inject()

    override fun onCreate(savedInstanceState: Bundle?) {
        installSplashScreen()
        super.onCreate(savedInstanceState)

        enableEdgeToEdge()
        consumeContactCard(intent)
        consumeInvite(intent)
        consumeChatOpen(intent)
        consumeUpdateOpen(intent)

        setContent {
            val appearance by AppearanceStore.appearance.collectAsState()
            PromtuzTheme(appearance = appearance) {
                androidx.compose.foundation.layout.Box(androidx.compose.ui.Modifier.fillMaxSize()) {
                AppNavigation(viewModel)
                MediaViewerHost()
                com.promtuz.chat.ui.camera.CameraOverlayHost()
                InviteBottomSheet(viewModel)
                RequiredUpdateHost()
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
        if (CoreBridge.shouldLaunchApp()) viewModel.navigator.openExternal(Routes.Chat(convHex, name))
    }
}
