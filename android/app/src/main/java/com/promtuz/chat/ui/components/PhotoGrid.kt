package com.promtuz.chat.ui.components

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.grid.GridCells
import androidx.compose.foundation.lazy.grid.LazyVerticalGrid
import androidx.compose.foundation.lazy.grid.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import androidx.core.content.ContextCompat
import coil.compose.AsyncImage
import com.promtuz.chat.R
import com.promtuz.chat.ui.appearance.LocalChatColors
import com.promtuz.chat.utils.media.GalleryItem
import com.promtuz.chat.utils.media.loadGallery

private fun galleryPermissions(): Array<String> = when {
    Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE -> arrayOf(
        Manifest.permission.READ_MEDIA_IMAGES,
        Manifest.permission.READ_MEDIA_VIDEO,
        Manifest.permission.READ_MEDIA_VISUAL_USER_SELECTED,
    )
    Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU -> arrayOf(
        Manifest.permission.READ_MEDIA_IMAGES,
        Manifest.permission.READ_MEDIA_VIDEO,
    )
    else -> arrayOf(Manifest.permission.READ_EXTERNAL_STORAGE)
}

private fun Context.granted(perm: String) =
    ContextCompat.checkSelfPermission(this, perm) == PackageManager.PERMISSION_GRANTED

/** (has any access at all) to (that access is the API 34+ partial/selected-only grant). */
private fun Context.galleryAccess(perms: Array<String>): Pair<Boolean, Boolean> {
    val any = perms.any { granted(it) }
    val partial = Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE &&
        granted(Manifest.permission.READ_MEDIA_VISUAL_USER_SELECTED) &&
        !granted(Manifest.permission.READ_MEDIA_IMAGES)
    return any to partial
}

private fun formatDuration(ms: Long): String {
    val totalSec = ms / 1000
    return "%d:%02d".format(totalSec / 60, totalSec % 60)
}

/**
 * Inline MediaStore grid for the attach panel's Photos tab. Falls back to the
 * permissionless system picker ([onOpenSystemPicker]) when gallery permission
 * isn't granted, so picking media never hard-depends on this permission.
 */
@Composable
fun PhotoGrid(onSend: (List<Uri>) -> Unit, onOpenSystemPicker: () -> Unit) {
    val context = LocalContext.current
    val perms = remember { galleryPermissions() }
    var access by remember { mutableStateOf(context.galleryAccess(perms)) }
    val (hasAccess, partial) = access

    // Bumped on every permission result so the grid reloads — notably after "Select
    // more" in API 34 partial access, where hasAccess stays true so it alone won't re-key.
    var reload by remember { mutableIntStateOf(0) }
    val launcher = rememberLauncherForActivityResult(ActivityResultContracts.RequestMultiplePermissions()) {
        access = context.galleryAccess(perms)
        reload++
    }

    if (!hasAccess) {
        Column(
            Modifier.fillMaxSize(),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.spacedBy(10.dp, Alignment.CenterVertically),
        ) {
            Button(onClick = { launcher.launch(perms) }) { Text("Allow photo access") }
            TextButton(onClick = onOpenSystemPicker) { Text("Open gallery") }
        }
        return
    }

    val items by produceState(initialValue = emptyList<GalleryItem>(), key1 = hasAccess, key2 = reload) {
        value = loadGallery(context)
    }
    val selection = remember { mutableStateListOf<Uri>() }

    Box(Modifier.fillMaxSize()) {
        LazyVerticalGrid(
            columns = GridCells.Fixed(3),
            contentPadding = PaddingValues(start = 2.dp, top = 2.dp, end = 2.dp, bottom = 88.dp),
            modifier = Modifier.fillMaxSize(),
        ) {
            items(items, key = { it.uri }) { item ->
                val order = selection.indexOf(item.uri)
                PhotoCell(item, order) {
                    if (order >= 0) selection.removeAt(order) else selection.add(item.uri)
                }
            }
        }

        // API 34 partial access already scopes the query to user-picked items —
        // this just re-opens the system chooser to add more to that set.
        if (partial) {
            TextButton(
                onClick = { launcher.launch(perms) },
                modifier = Modifier.align(Alignment.TopEnd).padding(10.dp),
            ) { Text("Select more") }
        }

        if (selection.isNotEmpty()) {
            SendFab(
                count = selection.size,
                onClick = { onSend(selection.toList()) },
                modifier = Modifier.align(Alignment.BottomEnd).padding(16.dp),
            )
        }
    }
}

@Composable
private fun PhotoCell(item: GalleryItem, order: Int, onToggle: () -> Unit) {
    val accent = LocalChatColors.current.accent
    val selected = order >= 0
    val shape = RoundedCornerShape(4.dp)

    Box(
        Modifier
            .aspectRatio(1f)
            .padding(1.dp)
            .clip(shape)
            .then(if (selected) Modifier.border(2.dp, accent, shape) else Modifier)
            .clickable(onClick = onToggle),
    ) {
        AsyncImage(
            model = item.uri,
            contentDescription = null,
            contentScale = ContentScale.Crop,
            modifier = Modifier.fillMaxSize(),
        )
        if (selected) Box(Modifier.fillMaxSize().background(Color.Black.copy(alpha = 0.35f)))

        if (item.isVideo) {
            Text(
                formatDuration(item.durationMs),
                style = MaterialTheme.typography.labelSmall,
                color = Color.White,
                modifier = Modifier
                    .align(Alignment.BottomStart)
                    .padding(4.dp)
                    .clip(RoundedCornerShape(4.dp))
                    .background(Color.Black.copy(alpha = 0.55f))
                    .padding(horizontal = 4.dp, vertical = 1.dp),
            )
        }

        if (selected) {
            Box(
                Modifier
                    .align(Alignment.TopEnd)
                    .padding(4.dp)
                    .size(20.dp)
                    .clip(CircleShape)
                    .background(accent),
                contentAlignment = Alignment.Center,
            ) {
                Text(
                    "${order + 1}",
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.onPrimary,
                )
            }
        }
    }
}

@Composable
private fun SendFab(count: Int, onClick: () -> Unit, modifier: Modifier = Modifier) {
    val accent = LocalChatColors.current.accent
    Box(
        modifier
            .clip(RoundedCornerShape(percent = 50))
            .background(accent)
            .clickable(onClick = onClick)
            .padding(horizontal = 18.dp, vertical = 12.dp),
        contentAlignment = Alignment.Center,
    ) {
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            Text(
                "$count",
                style = MaterialTheme.typography.labelLarge,
                color = MaterialTheme.colorScheme.onPrimary,
            )
            DrawableIcon(
                R.drawable.i_send,
                Modifier.size(18.dp),
                tint = MaterialTheme.colorScheme.onPrimary,
            )
        }
    }
}
