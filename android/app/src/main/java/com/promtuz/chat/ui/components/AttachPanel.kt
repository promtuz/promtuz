package com.promtuz.chat.ui.components

import androidx.compose.ui.graphics.Color
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.size
import androidx.annotation.DrawableRes
import androidx.compose.animation.animateColorAsState
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.selected
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.sp
import com.promtuz.chat.R
import com.promtuz.chat.ui.theme.Promtuz
import dev.chrisbanes.haze.hazeSource
import dev.chrisbanes.haze.rememberHazeState
import android.net.Uri
import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.tween
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.ime
import androidx.compose.foundation.layout.imeAnimationTarget
import androidx.compose.foundation.layout.navigationBars
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.runtime.snapshotFlow
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.unit.dp
import com.promtuz.chat.ui.appearance.LocalChatColors
import com.promtuz.chat.ui.util.freezeOnExit
import dev.chrisbanes.haze.HazeState
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.withTimeoutOrNull
import kotlin.math.roundToInt

/*
 * The region is as tall as whatever is moving. The panel snaps in under a leaving keyboard and
 * stays under an arriving one until it is covered, so the messages above never shift.
 */
@OptIn(ExperimentalLayoutApi::class)
@Composable
fun ComposerPanel(
    open: Boolean,
    closingToKeyboard: Boolean,
    haze: HazeState,
    onHideKeyboard: () -> Unit,
    content: @Composable () -> Unit,
) {
    val density = LocalDensity.current
    val ime = WindowInsets.ime.getBottom(density)
    val imeTarget = WindowInsets.imeAnimationTarget.getBottom(density)
    val nav = WindowInsets.navigationBars.getBottom(density)
    val keyboardMin = with(density) { 120.dp.roundToPx() }

    var learned by remember { mutableIntStateOf(0) }
    val target = rememberUpdatedState(imeTarget)
    val opened = rememberUpdatedState(open)
    LaunchedEffect(Unit) {
        snapshotFlow { target.value }.collect { if (it > keyboardMin && !opened.value) learned = it }
    }
    val panelH = if (learned > 0) learned else with(density) { 300.dp.roundToPx() }

    val presence = remember { Animatable(0f) }
    val live = rememberUpdatedState(ime)
    val hide = rememberUpdatedState(onHideKeyboard)
    LaunchedEffect(open, closingToKeyboard) {
        val keyboardUp = { target.value > keyboardMin && live.value >= target.value }
        when {
            open -> if (live.value > 0) {
                presence.snapTo(1f)
                hide.value()
            } else {
                hide.value()
                presence.animateTo(1f, tween(240))
            }
            closingToKeyboard -> {
                withTimeoutOrNull(250) { snapshotFlow { !keyboardUp() }.first { it } }
                val arrived = withTimeoutOrNull(1500) { snapshotFlow { keyboardUp() }.first { it } } != null
                if (arrived) presence.snapTo(0f) else presence.animateTo(0f, tween(240))
            }
            else -> presence.animateTo(0f, tween(240))
        }
    }

    val regionPx = maxOf(ime, (panelH * presence.value).roundToInt(), nav)
    Box(Modifier.fillMaxWidth().height(with(density) { regionPx.toDp() })) {
        if (presence.value > 0f) {
            Box(
                Modifier
                    .align(Alignment.BottomCenter)
                    .fillMaxWidth()
                    .height(with(density) { panelH.toDp() })
                    .freezeOnExit()
                    .frost(haze, FrostRole.Sheet, PanelShape),
            ) {
                content()
            }
        }
    }
}

private val PanelShape = RoundedCornerShape(topStart = 20.dp, topEnd = 20.dp)

@Composable
fun AttachPanelBody(
    allowPhotos: Boolean,
    allowFiles: Boolean,
    onPickPhotos: () -> Unit,
    onPickFiles: () -> Unit,
    onPickGalleryFiles: () -> Unit,
    onSendPhotos: (List<Uri>) -> Unit,
    onOpenCamera: () -> Unit,
) {
    var tab by remember(allowPhotos) { mutableStateOf(if (allowPhotos) 0 else 1) }
    val local = rememberHazeState()

    Box(Modifier.fillMaxSize()) {
        Box(Modifier.fillMaxSize().hazeSource(local)) {
            if (tab == 0) PhotoGrid(onSend = onSendPhotos, onOpenSystemPicker = onPickPhotos, onOpenCamera = onOpenCamera)
            else FilesTab(onPickFiles, onPickGalleryFiles.takeIf { allowPhotos })
        }
        if (allowPhotos && allowFiles) GlassTabBar(local, Modifier.align(Alignment.BottomCenter)) {
            GlassTab("Gallery", R.drawable.oi_image, tab == 0) { tab = 0 }
            GlassTab("Files", R.drawable.oi_file_attachment, tab == 1) { tab = 1 }
        }
    }
}


@Composable
private fun FilesTab(onBrowse: () -> Unit, onGalleryFiles: (() -> Unit)?) {
    Column(Modifier.fillMaxSize().padding(start = 12.dp, end = 12.dp, top = 12.dp)) {
        SettingsCard {
            SettingsRow("Browse files", icon = R.drawable.oi_file_attachment, titleColor = Promtuz.colors.accent, onClick = onBrowse)
            if (onGalleryFiles != null) SettingsRow(
                "Gallery as files", subtitle = "Send photos without compression",
                icon = R.drawable.oi_image, trailing = Trailing.Chevron, onClick = onGalleryFiles,
            )
        }
    }
}
