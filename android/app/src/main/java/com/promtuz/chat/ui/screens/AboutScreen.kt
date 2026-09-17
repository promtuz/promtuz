package com.promtuz.chat.ui.screens

import android.content.ActivityNotFoundException
import android.content.Intent
import android.widget.Toast
import androidx.compose.foundation.Image
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalLayoutDirection
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.core.net.toUri
import com.promtuz.chat.BuildConfig
import com.promtuz.chat.R
import com.promtuz.chat.presentation.viewmodel.UpdateVM
import com.promtuz.chat.ui.components.DrawableIcon
import com.promtuz.chat.ui.components.GroupedActionRow
import com.promtuz.chat.ui.components.SimpleScreen
import org.koin.androidx.compose.koinViewModel

@Composable
fun AboutScreen(onOpenLicenses: () -> Unit, updates: UpdateVM = koinViewModel()) {
    val context = LocalContext.current
    val direction = LocalLayoutDirection.current
    val channel by updates.channel.collectAsState()
    // Share the phone build even from an emulator. Updating this device still uses its own ABI.
    val downloadLink = "https://apt.promtuz.dev/apk/$channel/arm64-v8a/latest.apk"

    fun open(intent: Intent) {
        try {
            context.startActivity(intent)
        } catch (_: ActivityNotFoundException) {
            Toast.makeText(context, "No app can open this link", Toast.LENGTH_SHORT).show()
        }
    }

    SimpleScreen({ Text("About Promtuz") }) { padding ->
        LazyColumn(
            Modifier.fillMaxSize().padding(
                start = padding.calculateLeftPadding(direction),
                end = padding.calculateRightPadding(direction),
            ),
            contentPadding = PaddingValues(18.dp, padding.calculateTopPadding() + 24.dp, 18.dp, 32.dp),
            verticalArrangement = Arrangement.spacedBy(4.dp),
        ) {
            item("app") {
                Column(
                    Modifier.fillMaxWidth().padding(bottom = 32.dp),
                    horizontalAlignment = Alignment.CenterHorizontally,
                    verticalArrangement = Arrangement.spacedBy(10.dp),
                ) {
                    Image(painterResource(R.drawable.logo_colored), null, Modifier.size(88.dp))
                    Text("Promtuz", style = MaterialTheme.typography.headlineLarge)
                    Text(
                        "${BuildConfig.VERSION_NAME} · ${if (BuildConfig.DEBUG) "Debug" else "Release"}",
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                        style = MaterialTheme.typography.bodyMedium,
                    )
                    Text(
                        "Private messaging. Open source.",
                        Modifier.padding(top = 6.dp),
                        style = MaterialTheme.typography.bodyLarge,
                        textAlign = TextAlign.Center,
                    )
                }
            }
            item("share") {
                GroupedActionRow(
                    "Share Promtuz", 0, 3,
                    onClick = {
                        open(Intent.createChooser(
                            Intent(Intent.ACTION_SEND).setType("text/plain")
                                .putExtra(Intent.EXTRA_SUBJECT, "Promtuz for Android")
                                .putExtra(Intent.EXTRA_TEXT, "Download Promtuz for Android\n$downloadLink"),
                            "Share Promtuz",
                        ))
                    },
                    supportingText = "Android · ${channel.replaceFirstChar { it.uppercase() }}",
                ) { DrawableIcon(R.drawable.i_link, size = 26.dp) }
            }
            item("source") {
                GroupedActionRow("Source code", 1, 3, onClick = {
                    open(Intent(Intent.ACTION_VIEW, "https://github.com/promtuz/promtuz".toUri()))
                }) { DrawableIcon(R.drawable.i_code, size = 26.dp) }
            }
            item("licenses") {
                GroupedActionRow("Open source licenses", 2, 3, onOpenLicenses) {
                    DrawableIcon(R.drawable.i_info, size = 26.dp)
                }
            }
        }
    }
}
