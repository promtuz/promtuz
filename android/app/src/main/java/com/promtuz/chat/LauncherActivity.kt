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
import com.promtuz.chat.ui.appearance.AppearanceStore
import com.promtuz.chat.presentation.viewmodel.AppVM
import com.promtuz.chat.ui.components.InviteBottomSheet
import com.promtuz.chat.ui.theme.PromtuzTheme
import com.promtuz.chat.utils.InviteLink
import com.promtuz.core.CoreBridge
import org.koin.android.ext.android.inject

/**
 * The one app activity: hosts the whole nav stack. The start route (Welcome vs Home) is gated in
 * [AppVM] by [CoreBridge.shouldLaunchApp]. OS-boundary screens (manage-space) stay separate.
 */
class LauncherActivity : ComponentActivity() {
    private val viewModel: AppVM by inject()

    override fun onCreate(savedInstanceState: Bundle?) {
        installSplashScreen()
        super.onCreate(savedInstanceState)

        enableEdgeToEdge()
        consumeInvite(intent)

        setContent {
            val appearance by AppearanceStore.appearance.collectAsState()
            PromtuzTheme(appearance = appearance) {
                AppNavigation(viewModel)
                InviteBottomSheet(viewModel)
            }
        }
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        consumeInvite(intent)
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
}
