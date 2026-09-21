package com.promtuz.chat.ui.screens

import android.graphics.Bitmap
import android.net.Uri
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.gestures.detectTransformGestures
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.graphics.drawscope.withTransform
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.dp
import com.promtuz.chat.presentation.viewmodel.ProfileVM
import com.promtuz.chat.presentation.viewmodel.ProfileWork
import com.promtuz.chat.ui.components.AVATAR_RADIUS_RATIO
import com.promtuz.chat.ui.components.SimpleScreen
import com.promtuz.chat.utils.media.PhotoCrop
import com.promtuz.chat.utils.media.decodeDownscaled
import kotlinx.coroutines.CancellationException
import java.util.UUID

@Composable
fun ProfilePhotoScreen(uri: String, viewModel: ProfileVM, onSaved: () -> Unit) {
    val context = LocalContext.current
    var source by remember(uri) { mutableStateOf<Bitmap?>(null) }
    var loading by remember(uri) { mutableStateOf(true) }
    var centerX by rememberSaveable(uri) { mutableFloatStateOf(.5f) }
    var centerY by rememberSaveable(uri) { mutableFloatStateOf(.5f) }
    var zoom by rememberSaveable(uri) { mutableFloatStateOf(1f) }
    val crop = PhotoCrop(centerX, centerY, zoom)
    val work by viewModel.work.collectAsState()
    val busy = work == ProfileWork.Busy
    val requestId = rememberSaveable(uri) { UUID.randomUUID().toString() }
    val savedPhoto by viewModel.savedPhoto.collectAsState()
    val currentOnSaved by rememberUpdatedState(onSaved)
    // Survives rotation during Save, without delivering completion to a later editor.
    LaunchedEffect(savedPhoto) {
        if (savedPhoto == requestId) currentOnSaved()
    }
    LaunchedEffect(uri) {
        source = try {
            decodeDownscaled(context, Uri.parse(uri), 1024)
        } catch (e: CancellationException) {
            throw e
        } catch (_: Exception) {
            null
        }
        loading = false
    }

    SimpleScreen(
        title = { Text("Profile photo") },
        actions = {
            TextButton(
                enabled = source != null && !busy,
                onClick = { source?.let { bitmap ->
                    viewModel.savePicture(bitmap, crop, requestId)
                } },
            ) {
                if (busy) CircularProgressIndicator(Modifier.size(18.dp), strokeWidth = 2.dp)
                else Text("Save")
            }
        },
    ) { padding ->
        BoxWithConstraints(Modifier.fillMaxSize().padding(padding), contentAlignment = Alignment.Center) {
            val edge = minOf(maxWidth - 32.dp, maxHeight - 100.dp).coerceAtLeast(1.dp)
            Column(horizontalAlignment = Alignment.CenterHorizontally, verticalArrangement = Arrangement.spacedBy(16.dp)) {
                val bitmap = source
                if (bitmap != null) {
                    val image = remember(bitmap) { bitmap.asImageBitmap() }
                    Canvas(
                        Modifier.size(edge).clip(RoundedCornerShape(edge / AVATAR_RADIUS_RATIO))
                            .semantics { contentDescription = "Profile photo crop" }
                            .pointerInput(bitmap, busy) {
                                if (!busy) detectTransformGestures { centroid, pan, scale, _ ->
                                    val before = PhotoCrop(centerX, centerY, zoom)
                                    val nextZoom = (zoom * scale).coerceIn(1f, 5f)
                                    val oldSide = before.side(bitmap.width, bitmap.height)
                                    val newSide = minOf(bitmap.width, bitmap.height) / nextZoom
                                    val focal = centroid - Offset(size.width / 2f, size.height / 2f)
                                    val shift = focal * ((oldSide - newSide) / size.width.toFloat()) -
                                        pan * (newSide / size.width.toFloat())
                                    val after = PhotoCrop(
                                        centerX + shift.x / bitmap.width,
                                        centerY + shift.y / bitmap.height, nextZoom,
                                    ).bounded(bitmap.width, bitmap.height)
                                    centerX = after.centerX; centerY = after.centerY; zoom = after.zoom
                                }
                            },
                    ) {
                        val bounded = crop.bounded(bitmap.width, bitmap.height)
                        val scale = size.width / bounded.side(bitmap.width, bitmap.height)
                        withTransform({
                            translate(size.width / 2f - bounded.centerX * bitmap.width * scale,
                                size.height / 2f - bounded.centerY * bitmap.height * scale)
                            scale(scale, scale, pivot = Offset.Zero)
                        }) { drawImage(image) }
                    }
                    Text("Drag to move · Pinch to zoom", style = MaterialTheme.typography.bodyMedium)
                } else if (loading) CircularProgressIndicator()
                else Text("Couldn't read that photo. Choose another one.", Modifier.padding(horizontal = 24.dp))
                (work as? ProfileWork.Failed)?.let {
                    Text(it.reason, Modifier.padding(horizontal = 24.dp), color = MaterialTheme.colorScheme.error)
                }
            }
        }
    }
}
