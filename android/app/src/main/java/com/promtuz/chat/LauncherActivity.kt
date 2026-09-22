package com.promtuz.chat

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
import com.promtuz.chat.update.UpdateNotifier
import com.promtuz.chat.update.UpdateRepository
import com.promtuz.chat.ui.theme.PromtuzTheme
import com.promtuz.chat.utils.InviteLink
import com.promtuz.core.CoreBridge
import com.promtuz.core.push.PushNotifier
import org.koin.android.ext.android.inject

/**
 * The one app activity: hosts the whole nav stack. The start route (Welcome vs Home) is gated in
 * [AppVM] by [CoreBridge.shouldLaunchApp]. OS-boundary screens (manage-space) stay separate.
 */
class LauncherActivity : ComponentActivity() {
    private val viewModel: AppVM by inject()
    private val updates: UpdateRepository by inject()

    override fun onCreate(savedInstanceState: Bundle?) {
        installSplashScreen()
        super.onCreate(savedInstanceState)

        enableEdgeToEdge()
        consumeInvite(intent)
        consumeChatOpen(intent)
        consumeUpdateOpen(intent)

        setContent {
            val appearance by AppearanceStore.appearance.collectAsState()
            PromtuzTheme(appearance = appearance) {
                AppNavigation(viewModel)
                MediaViewerHost()
                com.promtuz.chat.ui.camera.CameraOverlayHost()
                InviteBottomSheet(viewModel)
            }
        }
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        consumeInvite(intent)
        consumeChatOpen(intent)
        consumeUpdateOpen(intent)
    }

    private fun consumeUpdateOpen(intent: Intent) {
        if (!intent.getBooleanExtra(UpdateNotifier.EXTRA_OPEN_UPDATE, false)) return
        intent.removeExtra(UpdateNotifier.EXTRA_OPEN_UPDATE)
        viewModel.navigator.openExternal(Routes.Updates)
        updates.check()
    }

    /**
     * Pull an invite from a `/pair` App Link ([Intent.getData]) or an internal EXTRA_INVITE hand-off
     * (QR scan). Raise the confirm sheet now if we're set up, else defer until enroll finishes.
     */
    private fun consumeInvite(intent: Intent) {
        val invite = intent.getByteArrayExtra(InviteLink.EXTRA_INVITE)
            ?: intent.data?.let(InviteLink::decode)
            ?: return
        intent.removeExtra(InviteLink.EXTRA_INVITE) // one-shot; survive recreation
        if (CoreBridge.shouldLaunchApp()) viewModel.showInvite(invite) else viewModel.pendingInvite = invite
    }

    /**
     * A message-notification tap carries the chat's id + name; open that thread
     * once we're set up. The activity is exported (it has to be — launcher and
     * app links), so any app can send these extras: the id is held to the
     * shape a real one has before it reaches a route that parses it.
     */
    private fun consumeChatOpen(intent: Intent) {
        val convHex = intent.getStringExtra(PushNotifier.EXTRA_CONVERSATION)
            ?.takeIf { it.matches(Regex("[0-9a-f]{32}")) } ?: return
        val name = intent.getStringExtra(PushNotifier.EXTRA_CONV_NAME).orEmpty()
        intent.removeExtra(PushNotifier.EXTRA_CONVERSATION) // one-shot; survive recreation
        intent.removeExtra(PushNotifier.EXTRA_CONV_NAME)
        if (CoreBridge.shouldLaunchApp()) viewModel.openChat(convHex, name)
    }
}
