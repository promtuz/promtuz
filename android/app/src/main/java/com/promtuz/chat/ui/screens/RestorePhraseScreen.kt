package com.promtuz.chat.ui.screens

import android.app.Activity
import android.view.WindowManager
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.input.KeyboardCapitalization
import androidx.compose.ui.unit.dp
import com.promtuz.chat.security.RecoveryStore
import kotlinx.coroutines.launch

/**
 * Channel B restore (IDENTITY_RECOVERY.md §5.2): type the 24 words + a
 * display name, become yourself again. The name is a fallback — if the
 * Auto-Backup blob is present its backed-up name wins. FLAG_SECURE while
 * a secret is on screen.
 */
@Composable
fun RestorePhraseScreen(onRestored: () -> Unit) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    val colors = MaterialTheme.colorScheme
    val typography = MaterialTheme.typography

    var phrase by remember { mutableStateOf("") }
    var name by remember { mutableStateOf("") }
    var error by remember { mutableStateOf<String?>(null) }
    var busy by remember { mutableStateOf(false) }

    // The typed phrase IS the private key — keep it out of screenshots/recents.
    val window = (context as? Activity)?.window
    DisposableEffect(Unit) {
        window?.addFlags(WindowManager.LayoutParams.FLAG_SECURE)
        onDispose { window?.clearFlags(WindowManager.LayoutParams.FLAG_SECURE) }
    }

    Column(
        Modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState())
            .imePadding()
            .padding(horizontal = 24.dp, vertical = 48.dp),
        verticalArrangement = Arrangement.Center
    ) {
        Text("Restore your identity", style = typography.headlineMedium)
        Spacer(Modifier.height(8.dp))
        Text(
            "Enter your 24-word recovery phrase. Chats reconnect on the next message.",
            style = typography.bodyMedium,
            color = colors.onSurfaceVariant
        )

        Spacer(Modifier.height(24.dp))
        OutlinedTextField(
            phrase,
            { phrase = it; error = null },
            Modifier.fillMaxWidth(),
            label = { Text("Recovery phrase") },
            minLines = 3,
            keyboardOptions = KeyboardOptions(
                capitalization = KeyboardCapitalization.None, autoCorrectEnabled = false
            ),
            isError = error != null,
            supportingText = { error?.let { Text(it, color = colors.error) } },
        )

        Spacer(Modifier.height(12.dp))
        OutlinedTextField(
            name,
            { name = it },
            Modifier.fillMaxWidth(),
            label = { Text("Display name") },
            supportingText = { Text("Used if no backup is found") },
            singleLine = true,
        )

        Spacer(Modifier.height(24.dp))
        Button(
            {
                val words = phrase.trim().split(Regex("\\s+"))
                when {
                    words.size != 24 -> error = "A recovery phrase is exactly 24 words (got ${words.size})"
                    name.isBlank() -> error = "Pick a display name"
                    else -> {
                        busy = true
                        scope.launch {
                            try {
                                RecoveryStore.restoreFromPhrase(context, words, name.trim())
                                onRestored()
                            } catch (e: Exception) {
                                error = e.message ?: "Restore failed"
                            } finally {
                                busy = false
                            }
                        }
                    }
                }
            },
            Modifier.fillMaxWidth(),
            enabled = !busy,
        ) {
            if (busy) CircularProgressIndicator(Modifier.height(18.dp)) else Text("Restore")
        }
    }
}
