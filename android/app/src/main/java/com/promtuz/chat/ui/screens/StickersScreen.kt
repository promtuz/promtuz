package com.promtuz.chat.ui.screens

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Text
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.unit.dp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LifecycleEventEffect
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import com.promtuz.chat.R
import com.promtuz.chat.ui.components.*
import com.promtuz.chat.ui.theme.Promtuz
import com.promtuz.chat.data.ChatPrefs
import com.promtuz.chat.domain.model.StickerRef
import com.promtuz.chat.presentation.viewmodel.StickersVM
import com.promtuz.chat.utils.media.StickerImage
import org.koin.androidx.compose.koinViewModel

@Composable
fun StickersScreen(
    onCreate: () -> Unit,
    onAddImages: (packHex: String) -> Unit,
    viewModel: StickersVM = koinViewModel(),
) {
    val packs by viewModel.packs.collectAsStateWithLifecycle()
    LifecycleEventEffect(Lifecycle.Event.ON_RESUME) { viewModel.refresh() }
    var sheet by remember { mutableStateOf<StickerRef?>(null) }

    val c = Promtuz.colors
    GroupedScreen({ Text("Stickers") }) { padding ->
        Column(
            Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(padding.listPadding(top = 6.dp, bottom = 24.dp)),
        ) {
            SettingsCard(header = "Your packs") {
                SettingsRow(
                    "Create a pack", titleColor = c.accent,
                    leading = { DrawableIcon(R.drawable.oi_sticker, size = 24.dp, tint = c.accent) },
                    onClick = onCreate,
                )
                packs.forEach { pack ->
                    SettingsRow(
                        pack.name,
                        subtitle = if (pack.stickers.size == 1) "1 sticker" else "${pack.stickers.size} stickers",
                        leading = {
                            Box(Modifier.size(44.dp).clip(RoundedCornerShape(com.promtuz.chat.ui.theme.Radii.small)).background(c.cardHigh), contentAlignment = Alignment.Center) {
                                pack.cover?.let { StickerImage(it, null, Modifier.size(32.dp), contentScale = ContentScale.Fit) }
                            }
                        },
                        leadingSize = 44.dp,
                        trailing = Trailing.Chevron,
                        onClick = { pack.cover?.let { sheet = it } },
                    )
                }
            }
            if (packs.isNotEmpty()) {
                CardGap()
                var columns by remember { mutableIntStateOf(ChatPrefs.stickerColumns) }
                SettingsCard(header = "Panel") {
                    SettingsChoiceRow(
                        "Stickers per row",
                        choices = (3..6).map { Choice(it, "$it") },
                        selected = columns,
                        onSelect = { columns = it; ChatPrefs.stickerColumns = it },
                    )
                }
            }
        }
    }

    sheet?.let { ref ->
        StickerPackSheet(
            ref = ref,
            viewModel = viewModel,
            onDismiss = { sheet = null },
            onAddImages = { pack -> sheet = null; onAddImages(pack) },
        )
    }
}
