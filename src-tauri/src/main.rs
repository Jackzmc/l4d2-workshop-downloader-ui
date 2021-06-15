#![cfg_attr(
  all(not(debug_assertions), target_os = "windows"),
  windows_subsystem = "windows"
)]

mod config;
mod logger;

use steam_workshop_api::{Workshop, WorkshopItem};
use std::path::PathBuf;
use regex::Regex;
use serde::{Deserialize, Serialize};
use tauri::{Manager, State, Window};
use futures::{StreamExt};
use std::{io::Write, time::{UNIX_EPOCH}};
use std::sync::{Arc, Mutex};

struct Data {
  settings: config::Settings,
  downloads: Arc<Mutex<config::Downloads>>,
  logger: logger::Logger
}

struct SplashscreenWindow(Arc<Mutex<Window>>);
struct MainWindow(Arc<Mutex<Window>>);



#[derive(Serialize, Deserialize)]
enum ItemType {
  Updateable,
  Managed,
  Unmanaged,
  Unknown,
  Workshop
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum File {
  Item { 
    item: WorkshopItem,
    item_type: ItemType
  },
  Unknown {
    item: UnknownFile,
    item_type: ItemType
  }
}

#[derive(Serialize, Deserialize)]
struct UnknownFile {
  publishedfileid: String,
  file_size: Option<u64>,
  time_updated: Option<u64>,
}

#[tauri::command]
fn get_items(state: tauri::State<'_, Data>) -> Result<Vec<File>, String> {
  let regex = Regex::new(r"([0-9]{7,})").unwrap();
  let mut unknown_ids = Vec::new();
  let fileids = match Workshop::get_vpks_in_folder(&state.settings.gamedir) {
    Ok(results) => {
      //Tries to find an ID to parse
      let mut fileids: Vec<String> = Vec::with_capacity(results.len());
      for filename in results.iter() {
        if let Some(mat) = regex.find(&filename) {
          fileids.push(filename[mat.start()..mat.end()].to_string());
        } else {
          //ItemType::Unknown
          let full_file = format!("{}.vpk", filename);
          if let Ok(metadata) = std::fs::metadata(&state.settings.gamedir.join(full_file)) {
            unknown_ids.push(UnknownFile {
              publishedfileid: filename.clone(), 
              file_size: Some(metadata.len()),
              time_updated: Some(metadata.modified().unwrap().duration_since(UNIX_EPOCH).expect("time went backwards").as_millis() as u64)
            });
          } else {
            unknown_ids.push(UnknownFile {
              publishedfileid: filename.clone(), 
              file_size: None,
              time_updated: None
            });
          }
        }
      }
      fileids
    },
    Err(err) => {
      state.logger.error("get_items", &format!("get_vpks_in_folder returnd error: {}\nDirectory: {:?}", err, state.settings.gamedir));
      return Err(err)
    }
  };

  if fileids.is_empty() {
    return Ok(Vec::new());
  }

  let mut files: Vec<File> = Vec::with_capacity(fileids.len());
  let details: Vec<WorkshopItem> = match Workshop::new(None).get_published_file_details(&fileids) {
    Ok(details) => details,
    Err(err) => { 
      state.logger.error("get_items", &format!("Failed to get normal item details: {}\nIDS: {:?}", err, fileids));
      return Err(err.to_string())
    }
  };
  
  let downloads = config::Downloads::load()?;
  
  for detail in details {
    //TODO: 1. Check if file is in downloads list
    //2. Check if file has an update
    match downloads.get_download(&detail.publishedfileid) {
      Some(download) => {
        let item_type = 
        if detail.time_updated > download.time_updated {
          ItemType::Updateable
        } else {
          ItemType::Managed
        };
        files.push(File::Item {
          item: detail,
          item_type
        });
      },
      None => {
        files.push(File::Item {
          item: detail,
          item_type: ItemType::Unmanaged,
        });
      }
    }
  }

  if let Ok(workshop_items) = get_workshop_items(&state) {
    for item in workshop_items {
      files.push(File::Item {
        item,
        item_type: ItemType::Workshop
      })
    }
  }
  
  for unknown in unknown_ids {
    files.push(File::Unknown {
      item: unknown,
      item_type: ItemType::Unknown,
    });
  }
  Ok(files)
}

fn get_workshop_items(state: &tauri::State<Data>) -> Result<Vec<WorkshopItem>, String>{
  let fileids = match Workshop::get_vpks_in_folder(&state.settings.gamedir.join("workshop").as_path()) {
    Ok(fileids) => fileids,
    Err(err) => {
      state.logger.error("get_workshop_items", &format!("Failed to get workshop items: {}", err));
      return Err(err)
    }
  };

  if fileids.is_empty() {
    return Ok(Vec::new());
  }

  match Workshop::new(None).get_published_file_details(&fileids) {
    Ok(details) => return Ok(details),
    Err(err) => { 
      state.logger.error("get_workshop_items", &format!("Failed to get workshop item details: {}", err));
      return Err(err.to_string())
    }
  };
}


#[derive(Serialize, Deserialize)]
struct UpdatePayload {
  publishedfileid: String,
  bytes_downloaded: usize,
  complete: bool
}

#[derive(Serialize, Deserialize)]
struct ErrorPayload {
  publishedfileid: Option<String>,
  error: String
}

#[tauri::command]
fn get_settings(state: tauri::State<Data>) -> config::Settings {
  state.settings.clone()
}

#[tauri::command]
fn save_settings(state: tauri::State<Data>, changed: config::Settings) -> Result<(), String> {
  match config::Settings::load() {
    Ok(mut settings) => {
      settings.telemetry = changed.telemetry;
      Ok(())
    },
    Err(err) => {
      state.logger.error("save_settings", &format!("Could not load settings: {}", err.to_string()));
      return Err(err.to_string());
    }
  }
}

#[tauri::command]
fn close_splashscreen(
  splashscreen: State<SplashscreenWindow>,
  main: State<MainWindow>,
) {
  // Close splashscreen
  splashscreen.0.lock().unwrap().close().unwrap();
  // Show main window
  main.0.lock().unwrap().show().unwrap();
}

#[tauri::command]
fn get_install_info(
  state: tauri::State<'_, Data>,
  id: String
) -> Option<config::DownloadEntry> {
  match state.downloads.lock().unwrap().get_download(&id) {
    Some(download) => Some(download.clone()),
    None => None
  }

}

#[tauri::command]
fn import_addon(
  state: tauri::State<'_, Data>,
  item: steam_workshop_api::WorkshopItem,
  is_workshop: bool
) -> Result<(), String> {
  let dest_folder = &state.settings.gamedir;
  let src_folder = if is_workshop { dest_folder.join("workshop") } else { state.settings.gamedir.clone() };

  let filename = format!("{}.vpk", &item.publishedfileid);
  let download = config::DownloadEntry::from_item(&item);

  state.logger.debug("import_addon", &format!("Moving {} from {} to {}", 
    filename, 
    src_folder.to_string_lossy(), 
    dest_folder.to_string_lossy()
  ));

  if is_workshop {
    if let Err(err) = std::fs::rename(src_folder.join(&filename), dest_folder.join(&filename)) {
      state.logger.error("import_addon", &format!("Moving import for {} error: {}", item.publishedfileid, err));
      return Err(err.to_string());
    }
  }
  let mut downloads = state.downloads.lock().unwrap();
  downloads.add_download(download);

  if let Err(err) = downloads.save() {
    state.logger.error("import_addon", &format!("Saving import for {} error: {}", item.publishedfileid, err));
    return Err(err.to_string());
  }
  state.logger.logp(logger::LogLevel::NORMAL, "import_addon", &format!("Imported item \"{}\" (id {}). IsWorkshop: {}", &item.title, item.publishedfileid, is_workshop));
  Ok(())
}


#[tauri::command]
async fn download_addon(window: Window, state: tauri::State<'_, Data>, item: steam_workshop_api::WorkshopItem) -> Result<(), String> {
  let config = &state.settings;
  let mut dest = {
    let fname = config.gamedir.join(format!("{}.vpk", item.publishedfileid));
    std::fs::File::create(fname).expect("Could not create file")
  };
  let mut downloaded: usize = 0;
  state.logger.logp(logger::LogLevel::NORMAL, "download_addons", &format!("Starting download of file \"{}\" (id {}) ({} bytes)", &item.title, item.publishedfileid, item.file_size));
  match reqwest::Client::new()
    .get(&item.file_url)
    .header("User-Agent", "L4D2-Workshop-Downloader")
    .send()
    .await
  {
    Ok(response) => {
      let mut stream = response.bytes_stream();
      let mut chunk_index: u8 = 0;
      while let Some(result) = stream.next().await {
        match result {
          Ok(chunk) => {
            if let Err(err) = dest.write(&chunk) {
              state.logger.error("download_addon", &format!("Write error for ID {}: {}", item.publishedfileid, err));
              println!("[{}] Write Error: {}", &item.publishedfileid, err);
              break;
            }
            downloaded += chunk.len();
            chunk_index += 1;
            if chunk_index > 100 {
              chunk_index = 0;
              window.emit("progress", UpdatePayload {
                publishedfileid: item.publishedfileid.clone(),
                bytes_downloaded: downloaded,
                complete: false
              }).ok();
            }
          },
          Err(err) => {
            window.emit("progress", ErrorPayload {
              publishedfileid: Some(item.publishedfileid.clone()),
              error: err.to_string()
            }).ok();
            state.logger.error("download_addon", &format!("Chunk failure for ID {}: {}", item.publishedfileid, err));
            println!("Download for {} failed:\n{}", item.title, &err); 
            return Err(err.to_string())
          }
        }
      }
      dest.flush().ok();
      window.emit("progress", UpdatePayload {
        publishedfileid: item.publishedfileid.clone(),
        bytes_downloaded: downloaded,
        complete: true
      }).ok();
      let entry = config::DownloadEntry::from_item(&item);
      let mut downloads = state.downloads.lock().unwrap();
      match downloads.get_id_index(&item.publishedfileid) {
        Some(index) => downloads.set_download(index, entry),
        None => downloads.add_download(entry)
      }
      state.logger.logp(logger::LogLevel::NORMAL, "download_addon", &format!("Downloaded file \"{}\" (id {}) ({} bytes)", &item.title, item.publishedfileid, item.file_size));
      return Ok(())
    },
    Err(err) => {
      println!("Download failure for {}: {}", &item, err);
      return Err(err.to_string())
    }
  }
}

fn main() {
  tauri::Builder::default()
  .setup(|app| {
    // set the splashscreen and main windows to be globally available with the tauri state API
    app.manage(SplashscreenWindow(Arc::new(Mutex::new(
      app.get_window("splashscreen").unwrap(),
    ))));
    app.manage(MainWindow(Arc::new(Mutex::new(
      app.get_window("main").unwrap(),
    ))));
    //TODO: Check if settings exists, if not, create new. exit on error (or send err)
    let logger = logger::Logger::new(config::get_appdir().join("downloader.log"));
    let settings = match config::Settings::load() {
      Ok(config) => config,
      Err(_e) => {
        let gamedir = prompt_game_dir();
        let mut settings = config::Settings::new(gamedir);
        if let Err(err) = settings.save() {
          logger.warn("setup", &format!("Could not save settings: {}", err));
        }
        settings
      }
    };
    if !settings.gamedir.exists() {
      logger.error("setup", &format!("Specified game directory folder \"{}\" does not exist", settings.gamedir.to_string_lossy()));
      std::process::exit(1);
    }
    let downloads = match config::Downloads::load() {
      Ok(downloads) => downloads,
      Err(_e) => {
        config::Downloads::new()
      }
    };
    app.manage(Data {
      settings,
      downloads: Arc::new(Mutex::new(downloads)),
      logger
    });
    Ok(())
  })
  .invoke_handler(tauri::generate_handler![
    get_items, 
    download_addon,
    get_settings,
    save_settings,
    close_splashscreen,
    import_addon,
    get_install_info,
  ])
  .run(tauri::generate_context!())
  .expect("error while running tauri application");
}

fn prompt_game_dir() -> PathBuf {
  if let Some(file_path) = tinyfiledialogs::open_file_dialog(
    "Choose where Left 4 Dead 2 is installed", 
    "",
    Some((&["left4dead2.exe"], "left4dead2.exe"))
  ) {
    let path = PathBuf::from(file_path)
    .parent()
    .unwrap()
    .join("left4dead2")
    .join("addons");
    if !path.exists() {
      std::fs::create_dir_all(&path).ok();
      println!("Warn: left4dead2/addons folder missing, creating..");
      return prompt_game_dir();
    }
    return path
  } else {
    eprintln!("Could not open file dialog");
    std::process::exit(1);
  }
}