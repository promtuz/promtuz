package com.promtuz.chat

import android.content.Intent
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.core.splashscreen.SplashScreen
import androidx.core.splashscreen.SplashScreen.Companion.installSplashScreen
import androidx.lifecycle.lifecycleScope
import com.promtuz.chat.security.KeyManager
import com.promtuz.chat.ui.activities.App
import com.promtuz.chat.ui.activities.Welcome
import com.promtuz.core.API
import kotlinx.coroutines.launch

class LauncherActivity : ComponentActivity() {
    private lateinit var keyManager: KeyManager

    override fun onCreate(savedInstanceState: Bundle?) {
        val splashScreen: SplashScreen = installSplashScreen()
        super.onCreate(savedInstanceState)

        var keepSplashOnScreen = true

        splashScreen.setKeepOnScreenCondition {
            keepSplashOnScreen
        }

        lifecycleScope.launch {
            try {
                if (API.shouldLaunchApp()) {
                    startActivity(
                        Intent(this@LauncherActivity, App::class.java)
                    )
                } else {
                    startActivity(
                        Intent(this@LauncherActivity, Welcome::class.java)
                    )
                }

                finish()
            } finally {
                keepSplashOnScreen = false
            }
        }
    }
}