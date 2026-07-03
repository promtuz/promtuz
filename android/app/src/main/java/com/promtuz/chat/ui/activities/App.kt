package com.promtuz.chat.ui.activities

import android.content.Intent
import android.os.Bundle
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.appcompat.app.AppCompatActivity
import com.promtuz.chat.navigation.AppNavigation
import com.promtuz.chat.presentation.viewmodel.AppVM
import com.promtuz.chat.ui.components.InviteBottomSheet
import com.promtuz.chat.ui.theme.PromtuzTheme
import com.promtuz.chat.utils.InviteLink
import org.koin.android.ext.android.inject

class App : AppCompatActivity() {
    private val viewModel: AppVM by inject()

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        enableEdgeToEdge()

        consumeInvite(intent)

        setContent {
            PromtuzTheme {
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

    /** Raise the confirmation sheet if this launch forwarded invite bytes. */
    private fun consumeInvite(intent: Intent) {
        val bytes = intent.getByteArrayExtra(InviteLink.EXTRA_INVITE) ?: return
        intent.removeExtra(InviteLink.EXTRA_INVITE) // one-shot; survive recreation
        viewModel.showInvite(bytes)
    }
}