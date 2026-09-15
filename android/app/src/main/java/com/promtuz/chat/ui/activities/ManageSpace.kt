package com.promtuz.chat.ui.activities

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import com.promtuz.chat.ui.screens.StorageScreen
import com.promtuz.chat.ui.screens.BackupRestoreScreen
import com.promtuz.chat.navigation.NavStage
import com.promtuz.chat.navigation.Routes
import androidx.navigation3.runtime.rememberNavBackStack
import androidx.navigation3.runtime.NavEntry
import com.promtuz.chat.ui.theme.PromtuzTheme

/** Android's Manage storage entry point shares the in-app screen. */
class ManageSpace : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        setContent {
            val stack = rememberNavBackStack(Routes.Storage)
            PromtuzTheme {
                NavStage(stack, { if (stack.size > 1) stack.removeLastOrNull() else finish() }) { route ->
                    NavEntry(route) {
                        when (route) {
                            Routes.BackupRestore -> BackupRestoreScreen()
                            is Routes.StorageChat -> StorageScreen(conversation = route.conversation, chatName = route.name)
                            else -> StorageScreen(
                                onOpenBackup = { if (stack.last() != Routes.BackupRestore) stack.add(Routes.BackupRestore) },
                                onOpenChat = { conversation, name ->
                                    val destination = Routes.StorageChat(conversation, name)
                                    if (stack.last() != destination) stack.add(destination)
                                },
                            )
                        }
                    }
                }
            }
        }
    }
}
