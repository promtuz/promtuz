package com.promtuz.chat.ui.screens

import androidx.compose.runtime.getValue
import androidx.compose.runtime.setValue
import android.text.format.Formatter
import androidx.activity.compose.BackHandler
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.slideInVertically
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.compose.LifecycleResumeEffect
import com.promtuz.chat.R
import com.promtuz.chat.presentation.viewmodel.UpdateVM
import com.promtuz.chat.ui.components.AppButton
import com.promtuz.chat.ui.components.cardFill
import com.promtuz.chat.ui.theme.Radii
import com.promtuz.chat.ui.components.RowSubtitleStyle
import com.promtuz.chat.ui.components.RowTitleStyle
import com.promtuz.chat.ui.constants.Tweens
import com.promtuz.chat.ui.theme.Promtuz
import com.promtuz.chat.update.UpdateManifest
import com.promtuz.chat.update.UpdateState
import org.koin.androidx.compose.koinViewModel

/** Covers the whole app while the installed build can no longer be used. */
@Composable
fun RequiredUpdateHost(viewModel: UpdateVM = koinViewModel()) {
    val required by viewModel.required.collectAsState()
    var shown by remember { mutableStateOf(required) }
    if (required != null) shown = required
    AnimatedVisibility(
        required != null,
        enter = fadeIn(Tweens.microInteraction(240)) + slideInVertically(Tweens.microInteraction(240)) { it / 12 },
        exit = fadeOut(Tweens.microInteraction(200)),
    ) {
        shown?.let { RequiredUpdateScreen(it, viewModel) }
    }
}

@Composable
private fun RequiredUpdateScreen(manifest: UpdateManifest, viewModel: UpdateVM) {
    val state by viewModel.state.collectAsState()
    val context = LocalContext.current
    val c = Promtuz.colors

    BackHandler { }
    LifecycleResumeEffect(viewModel) {
        viewModel.setScreenVisible(true)
        onPauseOrDispose { viewModel.setScreenVisible(false) }
    }

    // A Surface, not a Box: it sets the content colour and keeps touches off the app below.
    Surface(Modifier.fillMaxSize(), color = c.groupedPage) {
        Column(Modifier.fillMaxSize().systemBarsPadding().padding(horizontal = 24.dp)) {
            Column(
                Modifier.weight(1f).fillMaxWidth().verticalScroll(rememberScrollState()),
                verticalArrangement = Arrangement.Center,
                horizontalAlignment = Alignment.CenterHorizontally,
            ) {
                Image(painterResource(R.drawable.logo_colored), null, Modifier.size(72.dp))
                Spacer(Modifier.height(28.dp))
                Text("Update required", style = TextStyle(fontSize = 28.sp, lineHeight = 34.sp, fontWeight = FontWeight.Medium), color = c.text)
                Spacer(Modifier.height(10.dp))
                Text(
                    "This version can’t talk to the relays anymore. Update to keep sending and receiving messages. Your chats are safe on this phone.",
                    style = RowSubtitleStyle.copy(fontSize = 15.sp, lineHeight = 22.sp), color = c.muted, textAlign = TextAlign.Center,
                )
                Spacer(Modifier.height(28.dp))
                Column(
                    Modifier.fillMaxWidth().clip(RoundedCornerShape(Radii.card)).background(cardFill()).padding(16.dp),
                    verticalArrangement = Arrangement.spacedBy(10.dp),
                ) {
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        Text("Promtuz ${manifest.versionName}", Modifier.weight(1f), style = RowTitleStyle.copy(fontWeight = FontWeight.Medium), color = c.text)
                        Text(Formatter.formatShortFileSize(context, manifest.size), style = RowSubtitleStyle, color = c.muted)
                    }
                    val progress = when (val s = state) {
                        is UpdateState.Downloading -> s.progress
                        is UpdateState.Ready -> 1f
                        else -> 0f
                    }
                    LinearProgressIndicator({ progress }, Modifier.fillMaxWidth(),
                        color = c.accentFill, trackColor = c.switchOff, gapSize = 0.dp, drawStopIndicator = {})
                    val (line, bad) = when (val s = state) {
                        UpdateState.Checking -> "Checking…" to false
                        is UpdateState.Downloading -> "Downloading… ${(s.progress * 100).toInt()}%" to false
                        is UpdateState.Ready -> "Downloaded. Confirm the update in Android’s installer." to false
                        is UpdateState.Error -> s.message to true
                        else -> "Ready to download" to false
                    }
                    Text(line, style = RowSubtitleStyle, color = if (bad) c.danger else c.muted)
                }
            }
            val s = state
            AppButton(
                when (s) {
                    is UpdateState.Error -> "Try again"
                    is UpdateState.Downloading, is UpdateState.Ready -> "Install"
                    else -> "Download"
                },
                onClick = {
                    when (s) {
                        is UpdateState.Error -> viewModel.check()
                        is UpdateState.Ready -> viewModel.install(s.manifest, s.apk)
                        else -> viewModel.download(manifest)
                    }
                },
                modifier = Modifier.fillMaxWidth().padding(top = 12.dp, bottom = 16.dp),
                enabled = s !is UpdateState.Downloading,
                busy = s == UpdateState.Checking,
            )
        }
    }
    UpdateInstallPermissionDialog(state, viewModel)
}
