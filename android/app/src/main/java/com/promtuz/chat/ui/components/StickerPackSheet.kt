package com.promtuz.chat.ui.components

import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.grid.GridCells
import androidx.compose.foundation.lazy.grid.LazyVerticalGrid
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.rememberModalBottomSheetState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import com.promtuz.chat.domain.model.StickerRef
import com.promtuz.chat.presentation.viewmodel.StickersVM
import com.promtuz.chat.presentation.viewmodel.UiPackPreview
import kotlinx.coroutines.launch

/** Pack preview with install, remove and creator actions. */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun StickerPackSheet(
    ref: StickerRef,
    viewModel: StickersVM,
    onDismiss: () -> Unit,
    onAddImages: (packHex: String) -> Unit,
) {
    val colors = MaterialTheme.colorScheme
    var preview by remember(ref.packHex) { mutableStateOf<UiPackPreview?>(null) }
    var error by remember(ref.packHex) { mutableStateOf<String?>(null) }
    var busy by remember { mutableStateOf(false) }
    var attempt by remember { mutableIntStateOf(0) }
    val scope = rememberCoroutineScope()
    val packs by viewModel.packs.collectAsStateWithLifecycle()
    val kept = packs.any { it.packHex == ref.packHex }

    LaunchedEffect(ref.packHex, attempt) {
        error = null
        viewModel.preview(ref).onSuccess { preview = it }.onFailure { error = "Couldn’t load this pack" }
    }

    var dismissRequested by remember { mutableStateOf(false) }
    val currentBusy by rememberUpdatedState(busy)
    val sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true, confirmValueChange = { !currentBusy })
    LaunchedEffect(dismissRequested, busy) {
        if (dismissRequested && !busy) {
            sheetState.hide()
            onDismiss()
        }
    }

    ModalBottomSheet(
        onDismissRequest = { if (!busy) onDismiss() },
        sheetState = sheetState,
        contentWindowInsets = { WindowInsets(0, 0, 0, 0) },
    ) {
        Column(Modifier.fillMaxHeight(0.8f)) {
            val p = preview
            Row(
                Modifier.fillMaxWidth().padding(horizontal = 24.dp, vertical = 4.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Column(Modifier.weight(1f)) {
                    Text(
                        p?.name ?: "Sticker pack",
                        style = MaterialTheme.typography.titleLarge,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                    p?.let {
                        Text(
                            if (it.stickers.size == 1) "1 sticker" else "${it.stickers.size} stickers",
                            style = MaterialTheme.typography.bodyMedium,
                            color = colors.onSurfaceVariant,
                        )
                    }
                }
                if (p?.mine == true && p.stickers.size < 100) TextButton(onClick = { onAddImages(ref.packHex) }, enabled = !busy) {
                    Text("Add images")
                }
            }
            Box(Modifier.weight(1f).fillMaxWidth(), contentAlignment = Alignment.Center) {
                when {
                    p != null -> LazyVerticalGrid(
                        columns = GridCells.Adaptive(72.dp),
                        contentPadding = PaddingValues(horizontal = 12.dp, vertical = 8.dp),
                    ) {
                        items(p.stickers.size, key = { p.stickers[it].key }) { i -> StickerCell(p.stickers[i]) }
                    }
                    error != null -> Column(horizontalAlignment = Alignment.CenterHorizontally) {
                        Text(error!!, color = colors.error)
                        TextButton(onClick = { attempt++ }) { Text("Retry") }
                    }
                    else -> CircularProgressIndicator(Modifier.size(28.dp), strokeWidth = 2.5.dp)
                }
            }
            if (p != null) {
                if (error != null) Text(
                    error!!, Modifier.padding(horizontal = 24.dp), color = colors.error,
                    style = MaterialTheme.typography.bodyMedium,
                )
                val label = when {
                    busy -> if (kept) "Removing…" else "Adding…"
                    kept -> "Remove stickers"
                    else -> "Add stickers"
                }
                GroupActionButton(
                    label,
                    onClick = {
                        busy = true
                        error = null
                        scope.launch {
                            val result = if (kept) viewModel.remove(ref.packHex) else viewModel.install(ref)
                            busy = false
                            result.onSuccess { dismissRequested = true }
                                .onFailure { error = if (kept) "Couldn’t remove the pack" else "Couldn’t add the pack" }
                        }
                    },
                    enabled = !busy,
                    modifier = Modifier.fillMaxWidth().padding(16.dp),
                )
            }
        }
    }
}
