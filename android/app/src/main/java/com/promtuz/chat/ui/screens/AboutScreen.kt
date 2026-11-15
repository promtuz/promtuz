package com.promtuz.chat.ui.screens

import android.content.ActivityNotFoundException
import android.content.Intent
import android.widget.Toast
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.core.net.toUri
import com.promtuz.chat.BuildConfig
import com.promtuz.chat.R
import com.promtuz.chat.presentation.viewmodel.UpdateVM
import com.promtuz.chat.ui.components.*
import com.promtuz.chat.ui.theme.Promtuz
import org.koin.androidx.compose.koinViewModel

@Composable
fun AboutScreen(onOpenLicenses: () -> Unit, onOpenHistory: () -> Unit, updates: UpdateVM = koinViewModel()) {
    val context = LocalContext.current
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

    val c = Promtuz.colors
    GroupedScreen({ Text("About") }) { padding ->
        Column(
            Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(padding.listPadding(top = 12.dp, bottom = 24.dp)),
        ) {
            Column(
                Modifier.fillMaxWidth().padding(bottom = 24.dp),
                horizontalAlignment = Alignment.CenterHorizontally,
            ) {
                Image(painterResource(R.drawable.logo_colored), null, Modifier.size(88.dp))
                Text("Promtuz", Modifier.padding(top = 18.dp),
                    style = TextStyle(fontSize = 28.sp, lineHeight = 34.sp, fontWeight = FontWeight.SemiBold), color = c.text)
                Text("Private messaging. Open source.", Modifier.padding(top = 4.dp), style = RowValueStyle, color = c.muted)
                Text(
                    buildAnnotatedString {
                        withStyle(SpanStyle(color = c.text, fontWeight = FontWeight.Medium)) { append("Version ${BuildConfig.VERSION_NAME}") }
                        append(" · ${channelLabel(updates.installedChannel)}")
                    },
                    Modifier.padding(top = 14.dp).clip(RoundedCornerShape(14.dp)).background(c.card)
                        .padding(horizontal = 12.dp, vertical = 5.dp),
                    style = TextStyle(fontSize = 13.sp, lineHeight = 18.sp), color = c.muted,
                )
            }
            SettingsCard {
                SettingsRow("What’s new", icon = R.drawable.i_download, trailing = Trailing.Chevron, onClick = onOpenHistory)
                SettingsRow("Source code", subtitle = "github.com/promtuz", leading = {
                    MorphIcon(MorphGlyph.Code, null, Modifier.size(24.dp), tint = c.muted)
                }, leadingSize = 24.dp, trailing = Trailing.External, onClick = {
                    open(Intent(Intent.ACTION_VIEW, "https://github.com/promtuz/promtuz".toUri()))
                })
                SettingsRow("Open source licenses", icon = R.drawable.oi_info, trailing = Trailing.Chevron, onClick = onOpenLicenses)
            }
            CardGap()
            SettingsCard {
                SettingsRow("Share Promtuz", icon = R.drawable.oi_export, trailing = Trailing.Chevron, onClick = {
                    open(Intent.createChooser(
                        Intent(Intent.ACTION_SEND).setType("text/plain")
                            .putExtra(Intent.EXTRA_SUBJECT, "Promtuz for Android")
                            .putExtra(Intent.EXTRA_TEXT, "Download Promtuz for Android\n$downloadLink"),
                        "Share Promtuz",
                    ))
                })
            }
        }
    }
}

