package com.promtuz.chat.ui.activities

import android.os.Bundle
import androidx.activity.SystemBarStyle
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.appcompat.app.AppCompatActivity
import androidx.compose.ui.graphics.*
import com.promtuz.chat.domain.model.Chat
import com.promtuz.chat.domain.model.LastMessage
import com.promtuz.chat.presentation.viewmodel.ChatVM
import com.promtuz.chat.ui.screens.ChatScreen
import com.promtuz.chat.ui.theme.PromtuzTheme
import org.koin.androidx.viewmodel.ext.android.viewModel

class Chat : AppCompatActivity() {
    private val viewModel: ChatVM by viewModel()

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        val userIdentity =
            intent.getByteArrayExtra("user")?.takeIf { it.size == 32 } ?: return finish()
        val userName = intent.getStringExtra("name") ?: "Unknown"

        val chat = Chat(
            identity = userIdentity,
            nickname = userName,
            lastMessage = LastMessage(null, 0)
        )

        viewModel.init(userIdentity)

        enableEdgeToEdge(
            statusBarStyle = SystemBarStyle.light(
                Color.Transparent.toArgb(),
                Color.Transparent.toArgb(),
            ),
            navigationBarStyle = SystemBarStyle.light(
                Color.Transparent.toArgb(),
                Color.Transparent.toArgb(),
            ),
        )

        setContent {
            PromtuzTheme {
                ChatScreen(
                    chat,
                    viewModel,
                )
            }
        }
    }
}
