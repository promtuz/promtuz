package com.promtuz.chat

import android.content.Intent
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.core.splashscreen.SplashScreen
import androidx.core.splashscreen.SplashScreen.Companion.installSplashScreen
import androidx.lifecycle.lifecycleScope
import com.promtuz.chat.ui.activities.App
import com.promtuz.chat.ui.activities.Welcome
import com.promtuz.chat.utils.InviteLink
import com.promtuz.core.CoreBridge
import kotlinx.coroutines.launch

class LauncherActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        val splashScreen: SplashScreen = installSplashScreen()
        super.onCreate(savedInstanceState)

        var keepSplashOnScreen = true

        splashScreen.setKeepOnScreenCondition {
            keepSplashOnScreen
        }

        // A /pair deeplink (App Link or promtuz://pair) carries invite bytes in
        // its fragment. Decode here and forward them to whichever screen we gate
        // to; Welcome pairs after enroll completes (deferred deeplink).
        val invite = intent?.data?.let(InviteLink::decode)

        lifecycleScope.launch {
            try {
                val target = if (CoreBridge.shouldLaunchApp()) App::class.java else Welcome::class.java
                startActivity(
                    Intent(this@LauncherActivity, target).apply {
                        invite?.let { putExtra(InviteLink.EXTRA_INVITE, it) }
                    }
                )

                finish()
            } finally {
                keepSplashOnScreen = false
            }
        }
    }
}
