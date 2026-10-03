package com.promtuz.chat.ui.screens

import androidx.compose.foundation.Image
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.material3.SegmentedButton
import androidx.compose.material3.SegmentedButtonDefaults
import androidx.compose.material3.SingleChoiceSegmentedButtonRow
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.unit.dp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LifecycleEventEffect
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import com.promtuz.chat.data.ChatPrefs
import com.promtuz.chat.domain.model.StickerRef
import com.promtuz.chat.presentation.viewmodel.StickersVM
import com.promtuz.chat.ui.components.GroupedActionRow
import com.promtuz.chat.ui.components.MorphGlyph
import com.promtuz.chat.ui.components.MorphIcon
import com.promtuz.chat.ui.components.SettingsSection
import com.promtuz.chat.ui.components.SimpleScreen
import com.promtuz.chat.ui.components.StickerPackSheet
import com.promtuz.chat.ui.components.listPadding
import com.promtuz.chat.utils.media.rememberStickerBitmap
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

    SimpleScreen({ Text("Stickers") }) { padding ->
        LazyColumn(
            Modifier.fillMaxSize(),
            contentPadding = padding.listPadding(bottom = 48.dp),
            verticalArrangement = Arrangement.spacedBy(4.dp),
        ) {
            item {
                GroupedActionRow("Create a pack", 0, 1, onCreate) {
                    MorphIcon(MorphGlyph.Plus, null, Modifier.size(26.dp))
                }
            }
            if (packs.isNotEmpty()) {
                item { SettingsSection("Your packs", top = 16.dp) }
                itemsIndexed(packs, key = { _, p -> p.packHex }) { index, pack ->
                    GroupedActionRow(
                        pack.name, index, packs.size,
                        onClick = { pack.cover?.let { sheet = it } },
                        supportingText = if (pack.stickers.size == 1) "1 sticker" else "${pack.stickers.size} stickers",
                    ) {
                        Box(Modifier.size(26.dp), contentAlignment = Alignment.Center) {
                            pack.cover?.let { cover ->
                                rememberStickerBitmap(cover)?.let {
                                    Image(it, null, Modifier.size(26.dp), contentScale = ContentScale.Fit)
                                }
                            }
                        }
                    }
                }
                item { SettingsSection("Stickers per row") }
                item {
                    var columns by remember { mutableIntStateOf(ChatPrefs.stickerColumns) }
                    SingleChoiceSegmentedButtonRow(Modifier.fillMaxWidth().padding(top = 4.dp)) {
                        (3..6).forEachIndexed { i, n ->
                            SegmentedButton(
                                selected = columns == n,
                                onClick = { columns = n; ChatPrefs.stickerColumns = n },
                                shape = SegmentedButtonDefaults.itemShape(i, 4),
                            ) { Text("$n") }
                        }
                    }
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
