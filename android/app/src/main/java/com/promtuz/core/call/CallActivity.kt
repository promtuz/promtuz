package com.promtuz.core.call

import android.app.KeyguardManager
import android.content.Context
import android.content.Intent
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.lifecycle.lifecycleScope
import com.promtuz.chat.ui.screens.CallScreen
import com.promtuz.chat.ui.theme.PromtuzTheme
import com.promtuz.chat.ui.appearance.AppearanceStore
import kotlinx.coroutines.launch

/**
 * The full-screen call, shown over the lock screen for an incoming ring and
 * for the length of a call. It hosts only [CallScreen] and closes itself the
 * moment the call ends, so there is no call UI to get stuck in the back stack.
 */
class CallActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        showOverLockScreen()
        enableEdgeToEdge()

        // Close as soon as there is no call — whichever way it ended.
        lifecycleScope.launch {
            CallController.state.collect { if (it == null) finish() }
        }

        setContent {
            val appearance by AppearanceStore.appearance.collectAsState()
            PromtuzTheme(appearance = appearance) {
                val call by CallController.state.collectAsState()
                CallScreen(call)
            }
        }
    }

    override fun onUserLeaveHint() {
        // Keep a connected video call visible as a floating window when the
        // user leaves, the way a phone call does.
        val call = CallController.state.value
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O &&
            call?.video == true && call.phase == CallController.Phase.Connected
        ) {
            runCatching { enterPictureInPictureMode(android.app.PictureInPictureParams.Builder().build()) }
        }
    }

    private fun showOverLockScreen() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O_MR1) {
            setShowWhenLocked(true)
            setTurnScreenOn(true)
            (getSystemService(Context.KEYGUARD_SERVICE) as? KeyguardManager)
                ?.requestDismissKeyguard(this, null)
        } else {
            @Suppress("DEPRECATION")
            window.addFlags(
                android.view.WindowManager.LayoutParams.FLAG_SHOW_WHEN_LOCKED or
                    android.view.WindowManager.LayoutParams.FLAG_TURN_SCREEN_ON,
            )
        }
    }

    companion object {
        fun launch(context: Context) {
            val intent = Intent(context, CallActivity::class.java)
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP)
            context.startActivity(intent)
        }
    }
}
