package com.promtuz.chat.ui.screens

import com.promtuz.chat.ui.components.TextButton
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.FocusDirection
import androidx.compose.ui.platform.LocalFocusManager
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardCapitalization
import androidx.compose.ui.unit.dp
import com.promtuz.chat.navigation.LocalNavForeground
import com.promtuz.chat.presentation.viewmodel.ProfileVM
import com.promtuz.chat.presentation.viewmodel.ProfileWork
import com.promtuz.chat.ui.components.AppTextField
import com.promtuz.chat.ui.components.AppTextFieldCounter
import com.promtuz.chat.ui.components.SimpleScreen
import com.promtuz.chat.ui.components.listPadding

/** Shares the profile's activity-owned VM; an accepted save survives leaving this screen. */
@Composable
fun EditProfileScreen(viewModel: ProfileVM, onSaved: () -> Unit) {
    val profile by viewModel.profile.collectAsState()
    val work by viewModel.work.collectAsState()
    val busy = work == ProfileWork.Busy
    val foreground = LocalNavForeground.current
    val focus = LocalFocusManager.current
    var name by rememberSaveable { mutableStateOf(profile.name) }
    var bio by rememberSaveable { mutableStateOf(profile.bio) }
    var initialized by rememberSaveable { mutableStateOf(profile.identity.isNotEmpty()) }
    LaunchedEffect(Unit) {
        viewModel.dismissError()
        viewModel.refresh()
    }
    // A restored navigation entry may compose before the profile has loaded. Fill it once,
    // without replacing an existing draft when profile events arrive later.
    LaunchedEffect(profile) {
        if (!initialized && profile.identity.isNotEmpty()) {
            name = profile.name
            bio = profile.bio
            initialized = true
        }
    }
    val changed = name.trim() != profile.name || bio.trim() != profile.bio
    SimpleScreen(
        title = { Text("Edit profile") },
        connectionStatus = false,
        actions = {
            TextButton(enabled = foreground && initialized && !busy && name.isNotBlank() && changed,
                onClick = { viewModel.saveDetails(name.trim(), bio.trim(), onSaved) }) {
                Text(if (busy) "Saving…" else "Save")
            }
        },
    ) { padding ->
        LazyColumn(Modifier.fillMaxSize().consumeWindowInsets(padding).imePadding(),
            contentPadding = padding.listPadding(top = 16.dp),
            verticalArrangement = Arrangement.spacedBy(16.dp)) {
            item {
                AppTextField(
                    value = name,
                    onValueChange = { if (it.length <= 32) name = it },
                    label = "Name",
                    placeholder = "Your name",
                    enabled = initialized && !busy,
                    keyboardOptions = KeyboardOptions(capitalization = KeyboardCapitalization.Words,
                        imeAction = ImeAction.Next),
                    keyboardActions = KeyboardActions(onNext = { focus.moveFocus(FocusDirection.Next) }),
                )
            }
            item {
                AppTextField(
                    value = bio,
                    onValueChange = { if (it.length <= 160) bio = it },
                    label = "Bio",
                    placeholder = "About you",
                    enabled = initialized && !busy,
                    singleLine = false,
                    minLines = 3,
                    maxLines = 6,
                    suffix = { AppTextFieldCounter(bio.length, 160) },
                )
            }
            if (busy || (!initialized && work !is ProfileWork.Failed)) item { LinearProgressIndicator(Modifier.fillMaxWidth()) }
            (work as? ProfileWork.Failed)?.let { failure ->
                item { Text(failure.reason, color = MaterialTheme.colorScheme.error) }
            }
        }
    }
}
