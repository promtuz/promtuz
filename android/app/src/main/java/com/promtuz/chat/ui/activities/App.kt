package com.promtuz.chat.ui.activities

import android.os.Bundle
import androidx.activity.SystemBarStyle
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.appcompat.app.AppCompatActivity
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.toArgb
import com.promtuz.chat.navigation.AppNavigation
import com.promtuz.chat.presentation.viewmodel.AppVM
import com.promtuz.chat.ui.theme.PromtuzTheme
import org.koin.android.ext.android.inject

class App : AppCompatActivity() {
    private val viewModel: AppVM by inject()

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        enableEdgeToEdge()

        setContent {
            PromtuzTheme {
                AppNavigation(viewModel)
            }
        }
    }
}