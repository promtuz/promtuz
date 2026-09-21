package com.promtuz.chat.ui.screens

import android.net.Uri
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.PickVisualMediaRequest
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.dp
import com.promtuz.chat.R
import com.promtuz.chat.presentation.viewmodel.ProfileVM
import com.promtuz.chat.presentation.viewmodel.ProfileWork
import com.promtuz.chat.ui.components.AppDropMenu
import com.promtuz.chat.ui.components.Avatar
import com.promtuz.chat.ui.components.MenuAction
import com.promtuz.chat.ui.components.SimpleScreen

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ProfileScreen(viewModel: ProfileVM, onChoosePhoto: (Uri) -> Unit) {
    val profile by viewModel.profile.collectAsState()
    val work by viewModel.work.collectAsState()
    val busy = work == ProfileWork.Busy
    LaunchedEffect(Unit) { viewModel.refresh() }
    val picker = rememberLauncherForActivityResult(ActivityResultContracts.PickVisualMedia()) { uri ->
        if (uri != null) {
            viewModel.dismissError()
            onChoosePhoto(uri)
        }
    }
    val choose = {
        picker.launch(PickVisualMediaRequest(ActivityResultContracts.PickVisualMedia.ImageOnly))
    }
    val editLabel = stringResource(R.string.profile_photo_edit)
    val chooseLabel = stringResource(R.string.profile_photo_choose)
    val removeLabel = stringResource(R.string.profile_photo_remove)

    SimpleScreen({ Text("Profile") }) { padding ->
        LazyColumn(
            Modifier.fillMaxSize(),
            contentPadding = PaddingValues(
                start = 18.dp, end = 18.dp,
                top = padding.calculateTopPadding() + 20.dp,
                bottom = padding.calculateBottomPadding() + 24.dp,
            ),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            item {
                val tile: @Composable (onClick: (() -> Unit)?) -> Unit = { onClick ->
                    Box(Modifier.semantics { contentDescription = editLabel }) {
                        Avatar(profile.name, size = 96.dp, image = profile.picture, onClick = onClick)
                        Box(
                            Modifier.align(Alignment.BottomEnd).size(30.dp).clip(CircleShape)
                                .background(MaterialTheme.colorScheme.primary),
                            contentAlignment = Alignment.Center,
                        ) {
                            if (busy) CircularProgressIndicator(
                                Modifier.size(16.dp), color = MaterialTheme.colorScheme.onPrimary,
                                strokeWidth = 2.dp,
                            ) else Icon(
                                painterResource(R.drawable.oi_camera), null, Modifier.size(16.dp),
                                tint = MaterialTheme.colorScheme.onPrimary,
                            )
                        }
                    }
                }
                if (profile.picture != null && !busy) {
                    AppDropMenu(
                        anchor = { tile(null) },
                        groups = listOf(
                            listOf(MenuAction(chooseLabel, R.drawable.oi_image, onClick = choose)),
                            listOf(MenuAction(removeLabel, R.drawable.oi_trash, destructive = true) {
                                viewModel.removePicture()
                            }),
                        ),
                    )
                } else tile(if (busy) null else choose)
            }
            item { Text(profile.name, style = MaterialTheme.typography.titleLargeEmphasized) }
            (work as? ProfileWork.Failed)?.let { failure ->
                item { Text(failure.reason, color = MaterialTheme.colorScheme.error) }
            }
        }
    }
}
