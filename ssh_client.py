import os
import json
import shlex
import shutil
import stat
import tempfile
import threading
import tkinter as tk
from datetime import datetime
from tkinter import filedialog, messagebox, simpledialog

import customtkinter as ctk
import paramiko
from PIL import Image, ImageTk


ctk.set_appearance_mode("Dark")
ctk.set_default_color_theme("blue")

APP_DIR = os.path.dirname(os.path.abspath(__file__))
DEFAULT_LOGIN_FILES = [
    os.path.join(os.path.expanduser("~"), "ssh.txt"),
    os.path.join(APP_DIR, "ssh.txt"),
]
CONFIG_FILE = os.path.join(os.path.expanduser("~"), ".ssh_sftp_file_client_config.json")
STATE_FILE = os.path.join(os.path.expanduser("~"), ".ssh_sftp_file_client_state.json")
APP_TEMP_DIR = os.path.join(tempfile.gettempdir(), "ssh_sftp_file_client_temp")
PREVIEWABLE_EXTENSIONS = {".png", ".jpg", ".jpeg", ".tif", ".tiff", ".bmp", ".gif"}


class SSHClientApp(ctk.CTk):
    def __init__(self):
        super().__init__()

        self.title("SSH/SFTP File Client")
        self.geometry("1400x800")

        self.ssh_client = None
        self.sftp_client = None
        self.current_remote_path = "."
        self.remote_shell_cwd = "~"
        self.command_history = []
        self.command_history_index = None
        home_dir = os.path.expanduser("~")
        self.local_left_path = home_dir
        self.local_right_path = home_dir
        self.local_entries = {"left": [], "right": []}
        self.remote_entries = []
        self.local_filters = {"left": "", "right": ""}
        self.local_sort_modes = {"left": "name", "right": "name"}
        self.remote_filter = ""
        self.remote_sort_mode = "name"
        self.preview_windows = []
        self.transfer_lock = threading.Lock()
        self.transfer_active = False
        self.config = {}
        self.login_file_path = None
        self.local_bookmarks = {}
        self.remote_bookmarks = []
        self.created_temp_files = []

        self.load_config()
        self.prepare_temp_dir()
        self.setup_ui()
        self.load_login_file()
        self.load_state()
        self.update_prompt_label()
        self.refresh_local_list("left")
        self.refresh_local_list("right")
        self.bind_shortcuts()
        self.protocol("WM_DELETE_WINDOW", self.on_close)

    def setup_ui(self):
        self.grid_columnconfigure(0, weight=1)
        self.grid_rowconfigure(1, weight=1)

        self.conn_frame = ctk.CTkFrame(self)
        self.conn_frame.grid(row=0, column=0, padx=10, pady=10, sticky="ew")

        self.host_entry = ctk.CTkEntry(self.conn_frame, placeholder_text="Host")
        self.host_entry.pack(side="left", padx=5, pady=5)

        self.user_entry = ctk.CTkEntry(self.conn_frame, placeholder_text="Username")
        self.user_entry.pack(side="left", padx=5, pady=5)

        self.pass_entry = ctk.CTkEntry(self.conn_frame, placeholder_text="Password", show="*")
        self.pass_entry.pack(side="left", padx=5, pady=5)

        self.port_entry = ctk.CTkEntry(self.conn_frame, placeholder_text="Port", width=70)
        self.port_entry.insert(0, "22")
        self.port_entry.pack(side="left", padx=5, pady=5)

        self.connect_btn = ctk.CTkButton(self.conn_frame, text="Connect", command=self.connect_ssh)
        self.connect_btn.pack(side="left", padx=5, pady=5)

        self.reload_login_btn = ctk.CTkButton(self.conn_frame, text="Reload Login", command=self.load_login_file)
        self.reload_login_btn.pack(side="left", padx=5, pady=5)

        self.main_frame = ctk.CTkFrame(self)
        self.main_frame.grid(row=1, column=0, padx=10, pady=5, sticky="nsew")
        for column in range(3):
            self.main_frame.grid_columnconfigure(column, weight=1)
        self.main_frame.grid_rowconfigure(0, weight=1)

        self.local_left_frame, self.local_left_path_entry, self.local_left_filter_entry, self.local_left_sort_menu, self.local_left_bookmark_menu, self.local_left_status_label, self.local_left_listbox = self.create_local_pane(
            parent=self.main_frame,
            column=0,
            title="Local A",
            open_command=lambda: self.open_selected_local_file("left"),
            copy_command=lambda: self.copy_between_local_panes("left", "right"),
            move_command=lambda: self.move_between_local_panes("left", "right"),
            upload_command=lambda: self.upload_selected_local_file("left"),
            delete_command=lambda: self.delete_selected_local_file("left"),
            go_command=lambda: self.go_to_local_path("left"),
        )
        self.local_left_listbox.bind("<Double-1>", lambda event: self.on_local_double_click("left"))
        self.local_left_listbox.bind("<Return>", lambda event: self.on_local_double_click("left"))
        self.local_left_listbox.bind("<Button-3>", lambda event: self.show_local_context_menu(event, "left"))
        self.local_left_listbox.bind("<<ListboxSelect>>", lambda event: self.update_local_status("left"))
        self.local_left_filter_entry.bind("<KeyRelease>", lambda event: self.on_local_filter_change("left"))
        self.local_left_sort_menu.configure(command=lambda value: self.on_local_sort_change("left", value))
        self.local_left_bookmark_menu.configure(command=lambda value: self.on_local_bookmark_change("left", value))

        self.local_right_frame, self.local_right_path_entry, self.local_right_filter_entry, self.local_right_sort_menu, self.local_right_bookmark_menu, self.local_right_status_label, self.local_right_listbox = self.create_local_pane(
            parent=self.main_frame,
            column=1,
            title="Local B",
            open_command=lambda: self.open_selected_local_file("right"),
            copy_command=lambda: self.copy_between_local_panes("right", "left"),
            move_command=lambda: self.move_between_local_panes("right", "left"),
            upload_command=lambda: self.upload_selected_local_file("right"),
            delete_command=lambda: self.delete_selected_local_file("right"),
            go_command=lambda: self.go_to_local_path("right"),
        )
        self.local_right_listbox.bind("<Double-1>", lambda event: self.on_local_double_click("right"))
        self.local_right_listbox.bind("<Return>", lambda event: self.on_local_double_click("right"))
        self.local_right_listbox.bind("<Button-3>", lambda event: self.show_local_context_menu(event, "right"))
        self.local_right_listbox.bind("<<ListboxSelect>>", lambda event: self.update_local_status("right"))
        self.local_right_filter_entry.bind("<KeyRelease>", lambda event: self.on_local_filter_change("right"))
        self.local_right_sort_menu.configure(command=lambda value: self.on_local_sort_change("right", value))
        self.local_right_bookmark_menu.configure(command=lambda value: self.on_local_bookmark_change("right", value))

        self.remote_frame = ctk.CTkFrame(self.main_frame)
        self.remote_frame.grid(row=0, column=2, padx=5, pady=5, sticky="nsew")
        ctk.CTkLabel(self.remote_frame, text="Remote", font=("Arial", 14, "bold")).pack(pady=5)

        self.remote_path_frame = ctk.CTkFrame(self.remote_frame)
        self.remote_path_frame.pack(fill="x", padx=5)

        self.remote_path_entry = ctk.CTkEntry(self.remote_path_frame, placeholder_text="/remote/path")
        self.remote_path_entry.pack(side="left", fill="x", expand=True, padx=(0, 5))
        self.remote_path_entry.bind("<Return>", lambda event: self.go_to_remote_path())

        self.remote_go_btn = ctk.CTkButton(self.remote_path_frame, text="Go", width=50, command=self.go_to_remote_path)
        self.remote_go_btn.pack(side="left")

        self.remote_tools_frame = ctk.CTkFrame(self.remote_frame)
        self.remote_tools_frame.pack(fill="x", padx=5, pady=(5, 0))

        self.remote_filter_entry = ctk.CTkEntry(self.remote_tools_frame, placeholder_text="Filter")
        self.remote_filter_entry.pack(side="left", fill="x", expand=True, padx=(0, 5))
        self.remote_filter_entry.bind("<KeyRelease>", lambda event: self.on_remote_filter_change())

        self.remote_sort_menu = ctk.CTkOptionMenu(
            self.remote_tools_frame,
            values=["name", "date", "size"],
            command=self.on_remote_sort_change,
            width=90,
        )
        self.remote_sort_menu.set("name")
        self.remote_sort_menu.pack(side="left")

        self.remote_bookmark_menu = ctk.CTkOptionMenu(
            self.remote_tools_frame,
            values=["Bookmarks"],
            command=self.on_remote_bookmark_change,
            width=110,
        )
        self.remote_bookmark_menu.set("Bookmarks")
        self.remote_bookmark_menu.pack(side="left", padx=(5, 0))

        self.remote_listbox = tk.Listbox(self.remote_frame, bg="#2b2b2b", fg="white", selectbackground="#1f538d", font=("Consolas", 16))
        self.remote_listbox.pack(expand=True, fill="both", padx=5, pady=5)
        self.remote_listbox.bind("<Double-1>", self.on_remote_double_click)
        self.remote_listbox.bind("<Return>", self.on_remote_double_click)
        self.remote_listbox.bind("<Button-3>", self.show_remote_context_menu)
        self.remote_listbox.bind("<<ListboxSelect>>", lambda event: self.update_remote_status())

        self.remote_actions = ctk.CTkFrame(self.remote_frame)
        self.remote_actions.pack(fill="x", padx=5, pady=(0, 5))

        ctk.CTkButton(self.remote_actions, text="Download To A", command=lambda: self.download_selected_remote_file("left")).pack(side="left", padx=5, pady=5)
        ctk.CTkButton(self.remote_actions, text="Download To B", command=lambda: self.download_selected_remote_file("right")).pack(side="left", padx=5, pady=5)
        ctk.CTkButton(self.remote_actions, text="Download & Open", command=self.download_and_open_selected_remote_file).pack(side="left", padx=5, pady=5)

        self.remote_status_label = ctk.CTkLabel(self.remote_frame, text="Remote: no selection", anchor="w")
        self.remote_status_label.pack(fill="x", padx=5, pady=(0, 5))

        self.bottom_frame = ctk.CTkFrame(self)
        self.bottom_frame.grid(row=2, column=0, padx=10, pady=10, sticky="ew")
        self.bottom_frame.grid_columnconfigure(0, weight=1)

        self.log_text = ctk.CTkTextbox(self.bottom_frame, height=150)
        self.log_text.grid(row=0, column=0, columnspan=3, padx=5, pady=5, sticky="ew")

        self.prompt_label = ctk.CTkLabel(self.bottom_frame, text="linux-shell$ ", width=180, anchor="w")
        self.prompt_label.grid(row=1, column=0, padx=(5, 0), pady=5, sticky="w")

        self.cmd_entry = ctk.CTkEntry(self.bottom_frame, placeholder_text="Enter Linux command here...")
        self.cmd_entry.grid(row=1, column=1, padx=5, pady=5, sticky="ew")
        self.cmd_entry.bind("<Return>", lambda event: self.run_ssh_command())
        self.cmd_entry.bind("<Up>", self.show_previous_command)
        self.cmd_entry.bind("<Down>", self.show_next_command)

        self.run_btn = ctk.CTkButton(self.bottom_frame, text="Run", width=60, command=self.run_ssh_command)
        self.run_btn.grid(row=1, column=2, padx=5, pady=5)

        self.transfer_status_label = ctk.CTkLabel(self.bottom_frame, text="Transfers idle", anchor="w")
        self.transfer_status_label.grid(row=2, column=0, columnspan=3, padx=5, pady=(0, 5), sticky="ew")

        self.transfer_progress = ctk.CTkProgressBar(self.bottom_frame)
        self.transfer_progress.grid(row=3, column=0, columnspan=3, padx=5, pady=(0, 5), sticky="ew")
        self.transfer_progress.set(0)

        self.local_context_menu = tk.Menu(self, tearoff=0)
        self.local_context_menu.add_command(label="Copy Path", command=self.copy_selected_local_path_from_menu)
        self.local_context_menu.add_command(label="Rename", command=self.rename_selected_local_from_menu)
        self.local_context_menu.add_command(label="New Folder", command=self.create_local_folder_from_menu)
        self.local_context_menu.add_command(label="Delete", command=self.delete_selected_local_from_menu)
        self.local_context_menu.add_command(label="Refresh", command=self.refresh_selected_local_side_from_menu)

        self.remote_context_menu = tk.Menu(self, tearoff=0)
        self.remote_context_menu.add_command(label="Copy Path", command=self.copy_selected_remote_path_from_menu)
        self.remote_context_menu.add_command(label="Rename", command=self.rename_selected_remote_item)
        self.remote_context_menu.add_command(label="New Folder", command=self.create_remote_folder)
        self.remote_context_menu.add_command(label="Delete Remote", command=self.delete_selected_remote_file)
        self.remote_context_menu.add_command(label="Refresh", command=self.refresh_remote_list)

        self.context_menu_local_side = None

        self.refresh_bookmark_menus()
        self.log_message("Welcome to SSH/SFTP File Client. Login fields can load from ssh.txt.")

    def get_default_local_bookmarks(self):
        home = os.path.expanduser("~")
        return {
            "Home": home,
            "Desktop": os.path.join(home, "Desktop"),
            "Downloads": os.path.join(home, "Downloads"),
            "Documents": os.path.join(home, "Documents"),
        }

    def get_default_remote_bookmarks(self):
        username = self.user_entry.get().strip() if hasattr(self, "user_entry") else ""
        defaults = ["/"]
        if username:
            defaults.append(f"/home/{username}")
        defaults.append("/tmp")
        seen = set()
        result = []
        for item in defaults:
            if item and item not in seen:
                seen.add(item)
                result.append(item)
        return result

    def refresh_bookmark_menus(self):
        local_values = ["Bookmarks", *self.local_bookmarks.keys()]
        self.local_left_bookmark_menu.configure(values=local_values)
        self.local_right_bookmark_menu.configure(values=local_values)
        self.local_left_bookmark_menu.set("Bookmarks")
        self.local_right_bookmark_menu.set("Bookmarks")

        remote_values = ["Bookmarks", *self.remote_bookmarks]
        self.remote_bookmark_menu.configure(values=remote_values)
        self.remote_bookmark_menu.set("Bookmarks")

    def load_config(self):
        self.local_bookmarks = self.get_default_local_bookmarks()
        self.remote_bookmarks = ["/", "/tmp"]
        self.login_file_path = next((path for path in DEFAULT_LOGIN_FILES if os.path.exists(path)), DEFAULT_LOGIN_FILES[0])
        try:
            with open(CONFIG_FILE, "r", encoding="utf-8") as config_file:
                self.config = json.load(config_file)
        except Exception:
            self.config = {}

        self.login_file_path = self.config.get("login_file", self.login_file_path)
        self.local_bookmarks.update(self.config.get("local_bookmarks", {}))
        configured_remote_bookmarks = self.config.get("remote_bookmarks")
        if isinstance(configured_remote_bookmarks, list) and configured_remote_bookmarks:
            self.remote_bookmarks = configured_remote_bookmarks

    def create_local_pane(self, parent, column, title, open_command, copy_command, move_command, upload_command, delete_command, go_command):
        frame = ctk.CTkFrame(parent)
        frame.grid(row=0, column=column, padx=5, pady=5, sticky="nsew")
        ctk.CTkLabel(frame, text=title, font=("Arial", 14, "bold")).pack(pady=5)

        path_frame = ctk.CTkFrame(frame)
        path_frame.pack(fill="x", padx=5)

        path_entry = ctk.CTkEntry(path_frame, placeholder_text=r"C:\path\to\folder")
        path_entry.pack(side="left", fill="x", expand=True, padx=(0, 5))
        path_entry.bind("<Return>", lambda event: go_command())

        go_btn = ctk.CTkButton(path_frame, text="Go", width=50, command=go_command)
        go_btn.pack(side="left")

        tools_frame = ctk.CTkFrame(frame)
        tools_frame.pack(fill="x", padx=5, pady=(5, 0))

        filter_entry = ctk.CTkEntry(tools_frame, placeholder_text="Filter")
        filter_entry.pack(side="left", fill="x", expand=True, padx=(0, 5))

        sort_menu = ctk.CTkOptionMenu(
            tools_frame,
            values=["name", "date", "size"],
            width=90,
        )
        sort_menu.set("name")
        sort_menu.pack(side="left")

        bookmark_menu = ctk.CTkOptionMenu(
            tools_frame,
            values=["Bookmarks"],
            width=110,
        )
        bookmark_menu.set("Bookmarks")
        bookmark_menu.pack(side="left", padx=(5, 0))

        listbox = tk.Listbox(frame, bg="#2b2b2b", fg="white", selectbackground="#1f538d", font=("Consolas", 16))
        listbox.pack(expand=True, fill="both", padx=5, pady=5)

        actions = ctk.CTkFrame(frame)
        actions.pack(fill="x", padx=5, pady=(0, 5))

        ctk.CTkButton(actions, text="Copy -> Other", command=copy_command).pack(side="left", padx=5, pady=5)
        ctk.CTkButton(actions, text="Move -> Other", command=move_command).pack(side="left", padx=5, pady=5)
        ctk.CTkButton(actions, text="Upload", command=upload_command).pack(side="left", padx=5, pady=5)

        status_label = ctk.CTkLabel(frame, text=f"{title}: no selection", anchor="w")
        status_label.pack(fill="x", padx=5, pady=(0, 5))

        return frame, path_entry, filter_entry, sort_menu, bookmark_menu, status_label, listbox

    def load_login_file(self):
        try:
            with open(self.login_file_path, "r", encoding="utf-8") as login_file:
                values = [line.strip() for line in login_file.readlines() if line.strip()]
            if len(values) < 3:
                self.log_message(f"Login file needs at least 3 non-empty lines: {self.login_file_path}")
                return
            self.host_entry.delete(0, tk.END)
            self.host_entry.insert(0, values[0])
            self.user_entry.delete(0, tk.END)
            self.user_entry.insert(0, values[1])
            self.pass_entry.delete(0, tk.END)
            self.pass_entry.insert(0, values[2])
            if len(values) >= 4:
                self.port_entry.delete(0, tk.END)
                self.port_entry.insert(0, values[3])
            self.remote_bookmarks = self.config.get("remote_bookmarks", self.get_default_remote_bookmarks())
            self.refresh_bookmark_menus()
            self.log_message(f"Loaded login details from {self.login_file_path}.")
        except FileNotFoundError:
            self.remote_bookmarks = self.config.get("remote_bookmarks", self.get_default_remote_bookmarks())
            self.refresh_bookmark_menus()
            self.log_message(f"Login file not found: {self.login_file_path}")
        except Exception as exc:
            self.log_message(f"Could not load login file: {exc}")

    def log_message(self, msg):
        self.log_text.insert("end", f"> {msg}\n")
        self.log_text.see("end")

    def update_prompt_label(self):
        host = self.host_entry.get().strip() or "linux"
        user = self.user_entry.get().strip() or "user"
        cwd = self.remote_shell_cwd or "~"
        self.prompt_label.configure(text=f"{user}@{host}:{cwd}$ ")

    def append_terminal_output(self, text):
        if not text:
            return
        self.log_text.insert("end", f"{text}\n")
        self.log_text.see("end")

    def show_previous_command(self, event):
        if not self.command_history:
            return "break"
        if self.command_history_index is None:
            self.command_history_index = len(self.command_history) - 1
        else:
            self.command_history_index = max(0, self.command_history_index - 1)
        self.cmd_entry.delete(0, tk.END)
        self.cmd_entry.insert(0, self.command_history[self.command_history_index])
        return "break"

    def show_next_command(self, event):
        if not self.command_history:
            return "break"
        if self.command_history_index is None:
            return "break"
        self.command_history_index += 1
        if self.command_history_index >= len(self.command_history):
            self.command_history_index = None
            self.cmd_entry.delete(0, tk.END)
        else:
            self.cmd_entry.delete(0, tk.END)
            self.cmd_entry.insert(0, self.command_history[self.command_history_index])
        return "break"

    def sync_remote_shell_cwd(self, path):
        self.remote_shell_cwd = path or "~"
        self.current_remote_path = self.remote_shell_cwd
        self.after(0, self.update_prompt_label)

    def fetch_remote_pwd(self):
        if not self.ssh_client:
            return
        try:
            _, stdout, _ = self.ssh_client.exec_command("pwd")
            remote_pwd = stdout.read().decode(errors="replace").strip() or "~"
            self.sync_remote_shell_cwd(remote_pwd)
        except Exception as exc:
            self.log_message(f"Could not determine remote working directory: {exc}")

    def build_connect_kwargs(self, password):
        return {
            "username": self.user_entry.get().strip(),
            "password": password.strip() or None,
            "look_for_keys": False,
            "allow_agent": False,
            "timeout": 10,
            "banner_timeout": 10,
            "auth_timeout": 10,
        }

    def get_local_path(self, side):
        return self.local_left_path if side == "left" else self.local_right_path

    def set_local_path(self, side, value):
        if side == "left":
            self.local_left_path = value
        else:
            self.local_right_path = value

    def get_local_widgets(self, side):
        if side == "left":
            return self.local_left_path_entry, self.local_left_filter_entry, self.local_left_sort_menu, self.local_left_bookmark_menu, self.local_left_status_label, self.local_left_listbox
        return self.local_right_path_entry, self.local_right_filter_entry, self.local_right_sort_menu, self.local_right_bookmark_menu, self.local_right_status_label, self.local_right_listbox

    def bind_shortcuts(self):
        self.bind_all("<F5>", self.refresh_active_pane)
        self.bind_all("<Delete>", self.delete_active_selection)
        self.bind_all("<Control-c>", self.copy_active_path)

    def prepare_temp_dir(self):
        os.makedirs(APP_TEMP_DIR, exist_ok=True)
        self.cleanup_temp_dir()

    def cleanup_temp_dir(self):
        if not os.path.isdir(APP_TEMP_DIR):
            return
        for item in os.listdir(APP_TEMP_DIR):
            path = os.path.join(APP_TEMP_DIR, item)
            try:
                if os.path.isfile(path):
                    os.remove(path)
                elif os.path.isdir(path):
                    shutil.rmtree(path)
            except Exception:
                pass

    def cleanup_created_temp_files(self):
        for path in list(self.created_temp_files):
            try:
                if os.path.exists(path):
                    os.remove(path)
            except Exception:
                pass
        self.created_temp_files = []

    def on_close(self):
        self.save_state()
        self.cleanup_created_temp_files()
        self.destroy()

    def load_state(self):
        try:
            with open(STATE_FILE, "r", encoding="utf-8") as state_file:
                state = json.load(state_file)
        except Exception:
            return

        self.geometry(state.get("geometry", self.geometry()))
        self.local_left_path = state.get("local_left_path", self.local_left_path)
        self.local_right_path = state.get("local_right_path", self.local_right_path)
        self.current_remote_path = state.get("current_remote_path", self.current_remote_path)
        self.remote_shell_cwd = self.current_remote_path
        self.local_filters.update(state.get("local_filters", {}))
        self.local_sort_modes.update(state.get("local_sort_modes", {}))
        self.remote_filter = state.get("remote_filter", self.remote_filter)
        self.remote_sort_mode = state.get("remote_sort_mode", self.remote_sort_mode)

    def save_state(self):
        state = {
            "geometry": self.geometry(),
            "local_left_path": self.local_left_path,
            "local_right_path": self.local_right_path,
            "current_remote_path": self.current_remote_path,
            "local_filters": self.local_filters,
            "local_sort_modes": self.local_sort_modes,
            "remote_filter": self.remote_filter,
            "remote_sort_mode": self.remote_sort_mode,
        }
        try:
            with open(STATE_FILE, "w", encoding="utf-8") as state_file:
                json.dump(state, state_file, indent=2)
        except Exception as exc:
            self.log_message(f"Could not save state: {exc}")

    def sort_entries(self, entries, sort_mode):
        if sort_mode == "date":
            return sorted(entries, key=lambda entry: (not entry["is_directory"], -entry["mtime"], entry["name"].lower()))
        if sort_mode == "size":
            return sorted(entries, key=lambda entry: (not entry["is_directory"], -entry["raw_size"], entry["name"].lower()))
        return sorted(entries, key=lambda entry: (not entry["is_directory"], entry["name"].lower()))

    def filter_entries(self, entries, filter_text):
        if not filter_text:
            return entries
        needle = filter_text.lower()
        return [entry for entry in entries if needle in entry["name"].lower()]

    def on_local_filter_change(self, side):
        _, filter_entry, _, _, _, _ = self.get_local_widgets(side)
        self.local_filters[side] = filter_entry.get().strip()
        self.refresh_local_list(side)

    def on_local_sort_change(self, side, value):
        self.local_sort_modes[side] = value
        self.refresh_local_list(side)

    def on_remote_filter_change(self):
        self.remote_filter = self.remote_filter_entry.get().strip()
        self.refresh_remote_list()

    def on_remote_sort_change(self, value):
        self.remote_sort_mode = value
        self.refresh_remote_list()

    def on_local_bookmark_change(self, side, value):
        if value == "Bookmarks":
            return
        target = self.local_bookmarks.get(value)
        if target and os.path.isdir(target):
            self.set_local_path(side, target)
            self.refresh_local_list(side)

    def on_remote_bookmark_change(self, value):
        if value == "Bookmarks":
            return
        self.remote_path_entry.delete(0, tk.END)
        self.remote_path_entry.insert(0, value)
        self.go_to_remote_path()

    def set_transfer_progress(self, text, progress=None):
        self.after(0, lambda: self.transfer_status_label.configure(text=text))
        if progress is not None:
            self.after(0, lambda: self.transfer_progress.set(progress))

    def reset_transfer_progress(self):
        self.set_transfer_progress("Transfers idle", 0)

    def update_local_status(self, side):
        _, _, _, _, status_label, listbox = self.get_local_widgets(side)
        selection_index = self.get_selected_index(listbox)
        if selection_index is None:
            status_label.configure(text=f"Local {side.upper()}: no selection")
            return
        if selection_index == 0:
            status_label.configure(text=f"Local {side.upper()}: parent directory")
            return
        entry = self.local_entries[side][selection_index - 1]
        full_path = os.path.join(self.get_local_path(side), entry["name"])
        status_label.configure(text=f"{full_path} | {entry['modified']} | {entry['size']}")

    def update_remote_status(self):
        selection_index = self.get_selected_index(self.remote_listbox)
        if selection_index is None:
            self.remote_status_label.configure(text="Remote: no selection")
            return
        if selection_index == 0:
            self.remote_status_label.configure(text="Remote: parent directory")
            return
        entry = self.remote_entries[selection_index - 1]
        full_path = f"{self.current_remote_path}/{entry['name']}".replace("//", "/")
        self.remote_status_label.configure(text=f"{full_path} | {entry['modified']} | {entry['size']}")

    def refresh_active_pane(self, event=None):
        focus_widget = self.focus_get()
        if focus_widget == self.remote_listbox:
            self.refresh_remote_list()
        elif focus_widget == self.local_right_listbox:
            self.refresh_local_list("right")
        else:
            self.refresh_local_list("left")
        return "break"

    def delete_active_selection(self, event=None):
        focus_widget = self.focus_get()
        if focus_widget == self.remote_listbox:
            self.delete_selected_remote_file()
        elif focus_widget == self.local_right_listbox:
            self.delete_selected_local_file("right")
        elif focus_widget == self.local_left_listbox:
            self.delete_selected_local_file("left")
        return "break"

    def copy_active_path(self, event=None):
        focus_widget = self.focus_get()
        if focus_widget == self.remote_listbox:
            remote_path = self.get_selected_remote_path()
            if remote_path:
                self.copy_to_clipboard(remote_path)
        elif focus_widget == self.local_right_listbox:
            local_path = self.get_selected_local_path("right")
            if local_path:
                self.copy_to_clipboard(local_path)
        elif focus_widget == self.local_left_listbox:
            local_path = self.get_selected_local_path("left")
            if local_path:
                self.copy_to_clipboard(local_path)
        return "break"

    def get_selected_listbox_value(self, listbox):
        selection = listbox.curselection()
        if not selection:
            return None
        return listbox.get(selection[0])

    def get_selected_index(self, listbox):
        selection = listbox.curselection()
        if not selection:
            return None
        return selection[0]

    def format_timestamp(self, timestamp):
        try:
            return datetime.fromtimestamp(timestamp).strftime("%Y-%m-%d %H:%M")
        except Exception:
            return "-"

    def format_size(self, size):
        if size in (None, ""):
            return "-"
        units = ["B", "KB", "MB", "GB", "TB"]
        value = float(size)
        unit_index = 0
        while value >= 1024 and unit_index < len(units) - 1:
            value /= 1024
            unit_index += 1
        if unit_index == 0:
            return f"{int(value)} {units[unit_index]}"
        return f"{value:.1f} {units[unit_index]}"

    def build_display_row(self, name, modified, size, is_directory):
        label = f"[DIR] {name}" if is_directory else name
        return f"{label:<40}  {modified:<16}  {size:>9}"

    def get_selected_local_path(self, side):
        _, _, _, _, _, listbox = self.get_local_widgets(side)
        selection_index = self.get_selected_index(listbox)
        if selection_index is None:
            return None
        if selection_index == 0:
            return None
        entries = self.local_entries[side]
        if selection_index - 1 >= len(entries):
            return None
        return os.path.join(self.get_local_path(side), entries[selection_index - 1]["name"])

    def get_remote_selected_name(self):
        selection_index = self.get_selected_index(self.remote_listbox)
        if selection_index is None or selection_index == 0:
            return None
        if selection_index - 1 >= len(self.remote_entries):
            return None
        entry = self.remote_entries[selection_index - 1]
        return entry["name"], entry["is_directory"]

    def get_selected_remote_path(self):
        remote_selection = self.get_remote_selected_name()
        if not remote_selection:
            return None
        item_name, _ = remote_selection
        return f"{self.current_remote_path}/{item_name}".replace("//", "/")

    def is_previewable_remote_file(self, filename):
        return os.path.splitext(filename)[1].lower() in PREVIEWABLE_EXTENSIONS

    def open_file_in_system(self, path):
        if not os.path.exists(path):
            self.log_message(f"File not found: {path}")
            return
        try:
            os.startfile(path)
            self.log_message(f"Opened {os.path.basename(path)}.")
        except Exception as exc:
            self.log_message(f"Open failed: {exc}")
            messagebox.showerror("Open failed", f"Could not open file: {exc}")

    def copy_to_clipboard(self, value):
        self.clipboard_clear()
        self.clipboard_append(value)
        self.update()
        self.log_message(f"Copied path: {value}")

    def create_app_temp_file(self, suffix):
        fd, path = tempfile.mkstemp(prefix="remote_", suffix=suffix, dir=APP_TEMP_DIR)
        os.close(fd)
        self.created_temp_files.append(path)
        return path

    def confirm_local_overwrite(self, path):
        if not os.path.exists(path):
            return True
        return messagebox.askyesno("Overwrite?", f"{path}\n\nalready exists. Overwrite it?")

    def remote_path_exists(self, path):
        try:
            self.sftp_client.stat(path)
            return True
        except Exception:
            return False

    def confirm_remote_overwrite(self, path):
        if not self.remote_path_exists(path):
            return True
        return messagebox.askyesno("Overwrite Remote?", f"{path}\n\nalready exists on the server. Overwrite it?")

    def prompt_new_name(self, title, initial_value):
        new_name = simpledialog.askstring(title, "Enter name:", initialvalue=initial_value, parent=self)
        if not new_name:
            return None
        return new_name.strip()

    def copy_selected_local_path_from_menu(self):
        if not self.context_menu_local_side:
            return
        local_path = self.get_selected_local_path(self.context_menu_local_side)
        if local_path:
            self.copy_to_clipboard(local_path)

    def delete_selected_local_from_menu(self):
        if self.context_menu_local_side:
            self.delete_selected_local_file(self.context_menu_local_side)

    def rename_selected_local_from_menu(self):
        if not self.context_menu_local_side:
            return
        self.rename_selected_local_item(self.context_menu_local_side)

    def create_local_folder_from_menu(self):
        if not self.context_menu_local_side:
            return
        self.create_local_folder(self.context_menu_local_side)

    def refresh_selected_local_side_from_menu(self):
        if self.context_menu_local_side:
            self.refresh_local_list(self.context_menu_local_side)

    def copy_selected_remote_path_from_menu(self):
        remote_path = self.get_selected_remote_path()
        if remote_path:
            self.copy_to_clipboard(remote_path)

    def show_local_context_menu(self, event, side):
        listbox = self.local_left_listbox if side == "left" else self.local_right_listbox
        clicked_index = listbox.nearest(event.y)
        if clicked_index < 0:
            return
        listbox.selection_clear(0, tk.END)
        listbox.selection_set(clicked_index)
        self.context_menu_local_side = side
        self.local_context_menu.tk_popup(event.x_root, event.y_root)

    def show_remote_context_menu(self, event):
        clicked_index = self.remote_listbox.nearest(event.y)
        if clicked_index < 0:
            return
        self.remote_listbox.selection_clear(0, tk.END)
        self.remote_listbox.selection_set(clicked_index)
        self.remote_context_menu.tk_popup(event.x_root, event.y_root)

    def show_image_preview(self, image_path, title):
        try:
            image = Image.open(image_path)
            image.thumbnail((900, 700))
            preview_window = ctk.CTkToplevel(self)
            preview_window.title(title)
            preview_window.geometry(f"{max(image.width + 20, 300)}x{max(image.height + 20, 200)}")

            photo = ImageTk.PhotoImage(image)
            label = tk.Label(preview_window, image=photo, bg="#1f1f1f")
            label.image = photo
            label.pack(expand=True, fill="both", padx=10, pady=10)

            self.preview_windows.append(preview_window)
            self.log_message(f"Opened preview for {title}.")
        except Exception as exc:
            self.log_message(f"Preview failed: {exc}")
            messagebox.showerror("Preview failed", f"Could not preview image: {exc}")

    def connect_ssh(self):
        host = self.host_entry.get().strip()
        password = self.pass_entry.get()
        port = int(self.port_entry.get())

        def _connect():
            try:
                self.log_message(f"Connecting to {host}...")
                self.ssh_client = paramiko.SSHClient()
                self.ssh_client.set_missing_host_key_policy(paramiko.AutoAddPolicy())
                self.ssh_client.connect(host, port=port, **self.build_connect_kwargs(password))
                self.sftp_client = self.ssh_client.open_sftp()
                self.fetch_remote_pwd()
                self.log_message("Connected successfully.")
                self.refresh_remote_list()
            except Exception as exc:
                self.log_message(f"Connection failed: {exc}")
                self.after(0, lambda: messagebox.showerror("Error", f"Failed to connect: {exc}"))

        threading.Thread(target=_connect, daemon=True).start()

    def refresh_local_list(self, side):
        path_entry, filter_entry, sort_menu, bookmark_menu, _, listbox = self.get_local_widgets(side)
        current_path = self.get_local_path(side)
        path_entry.delete(0, tk.END)
        path_entry.insert(0, current_path)
        filter_entry.delete(0, tk.END)
        filter_entry.insert(0, self.local_filters[side])
        sort_menu.set(self.local_sort_modes[side])
        bookmark_menu.set("Bookmarks")
        listbox.delete(0, tk.END)
        listbox.insert(tk.END, self.build_display_row(".. [Parent Directory]", "-", "-", True))
        entries = []
        try:
            for item in sorted(os.listdir(current_path), key=str.lower):
                full_path = os.path.join(current_path, item)
                is_directory = os.path.isdir(full_path)
                stats = os.stat(full_path)
                modified = self.format_timestamp(stats.st_mtime)
                size = "-" if is_directory else self.format_size(stats.st_size)
                entries.append(
                    {
                        "name": item,
                        "is_directory": is_directory,
                        "modified": modified,
                        "size": size,
                        "mtime": stats.st_mtime,
                        "raw_size": 0 if is_directory else stats.st_size,
                    }
                )
            filtered_entries = self.filter_entries(entries, self.local_filters[side])
            self.local_entries[side] = self.sort_entries(filtered_entries, self.local_sort_modes[side])
            for entry in self.local_entries[side]:
                listbox.insert(tk.END, self.build_display_row(entry["name"], entry["modified"], entry["size"], entry["is_directory"]))
        except Exception as exc:
            self.local_entries[side] = []
            self.log_message(f"Error reading local dir: {exc}")
        self.update_local_status(side)

    def refresh_remote_list(self):
        if not self.sftp_client:
            return
        self.remote_path_entry.delete(0, tk.END)
        self.remote_path_entry.insert(0, self.current_remote_path)
        self.remote_filter_entry.delete(0, tk.END)
        self.remote_filter_entry.insert(0, self.remote_filter)
        self.remote_sort_menu.set(self.remote_sort_mode)
        self.remote_listbox.delete(0, tk.END)
        self.remote_listbox.insert(tk.END, self.build_display_row(".. [Parent Directory]", "-", "-", True))
        entries = []
        try:
            files = self.sftp_client.listdir_attr(self.current_remote_path)
            for entry in sorted(files, key=lambda item: item.filename.lower()):
                is_directory = stat.S_ISDIR(entry.st_mode)
                modified = self.format_timestamp(entry.st_mtime)
                size = "-" if is_directory else self.format_size(entry.st_size)
                entries.append(
                    {
                        "name": entry.filename,
                        "is_directory": is_directory,
                        "modified": modified,
                        "size": size,
                        "mtime": entry.st_mtime,
                        "raw_size": 0 if is_directory else entry.st_size,
                    }
                )
            filtered_entries = self.filter_entries(entries, self.remote_filter)
            self.remote_entries = self.sort_entries(filtered_entries, self.remote_sort_mode)
            for entry in self.remote_entries:
                self.remote_listbox.insert(tk.END, self.build_display_row(entry["name"], entry["modified"], entry["size"], entry["is_directory"]))
        except Exception as exc:
            self.remote_entries = []
            self.log_message(f"Error reading remote dir: {exc}")
        self.update_remote_status()

    def run_ssh_command(self):
        cmd = self.cmd_entry.get().strip()
        if not cmd or not self.ssh_client:
            return
        self.cmd_entry.delete(0, tk.END)
        if not self.command_history or self.command_history[-1] != cmd:
            self.command_history.append(cmd)
        self.command_history_index = None

        def _run():
            try:
                self.after(0, lambda: self.append_terminal_output(f"{self.prompt_label.cget('text')}{cmd}"))
                quoted_cwd = shlex.quote(self.remote_shell_cwd or "~")
                remote_command = (
                    f"cd -- {quoted_cwd} 2>/dev/null || exit 1; "
                    f"{cmd}; "
                    "status=$?; "
                    "printf '\\n__CODEX_PWD__%s\\n' \"$PWD\"; "
                    "printf '__CODEX_EXIT__%s\\n' \"$status\""
                )
                _, stdout, stderr = self.ssh_client.exec_command(remote_command)
                out = stdout.read().decode(errors="replace")
                err = stderr.read().decode(errors="replace").strip()

                new_cwd = self.remote_shell_cwd
                exit_code = None
                output_lines = []
                for line in out.splitlines():
                    if line.startswith("__CODEX_PWD__"):
                        new_cwd = line.replace("__CODEX_PWD__", "", 1).strip() or self.remote_shell_cwd
                    elif line.startswith("__CODEX_EXIT__"):
                        exit_code = line.replace("__CODEX_EXIT__", "", 1).strip()
                    else:
                        output_lines.append(line)

                if new_cwd:
                    self.sync_remote_shell_cwd(new_cwd)
                if output_lines:
                    self.after(0, lambda: self.append_terminal_output("\n".join(output_lines)))
                if err:
                    self.after(0, lambda: self.append_terminal_output(err))
                if exit_code not in (None, "0"):
                    self.after(0, lambda: self.append_terminal_output(f"[exit {exit_code}]"))
            except Exception as exc:
                self.after(0, lambda: self.append_terminal_output(f"Command failed: {exc}"))

        threading.Thread(target=_run, daemon=True).start()

    def on_local_double_click(self, side):
        _, _, _, _, _, listbox = self.get_local_widgets(side)
        selection_index = self.get_selected_index(listbox)
        if selection_index is None:
            return
        current_path = self.get_local_path(side)
        if selection_index == 0:
            self.set_local_path(side, os.path.dirname(current_path) or current_path)
        else:
            entry = self.local_entries[side][selection_index - 1]
            new_path = os.path.join(current_path, entry["name"])
            if entry["is_directory"]:
                self.set_local_path(side, new_path)
            else:
                self.open_file_in_system(new_path)
        self.refresh_local_list(side)

    def go_to_local_path(self, side):
        path_entry, _, _, _, _, _ = self.get_local_widgets(side)
        requested_path = path_entry.get().strip()
        if not requested_path:
            return
        normalized_path = os.path.abspath(os.path.expanduser(requested_path))
        if not os.path.isdir(normalized_path):
            messagebox.showerror("Invalid path", f"Folder not found:\n{normalized_path}")
            self.refresh_local_list(side)
            return
        self.set_local_path(side, normalized_path)
        self.refresh_local_list(side)

    def on_remote_double_click(self, event):
        if not self.sftp_client:
            return
        selection_index = self.get_selected_index(self.remote_listbox)
        if selection_index is None:
            return
        if selection_index == 0:
            self.current_remote_path = os.path.dirname(self.current_remote_path) or "/"
        else:
            entry = self.remote_entries[selection_index - 1]
            if entry["is_directory"]:
                self.current_remote_path = os.path.join(self.current_remote_path, entry["name"]).replace("\\", "/")
            else:
                self.open_remote_file(entry["name"])
        self.sync_remote_shell_cwd(self.current_remote_path)
        self.refresh_remote_list()

    def go_to_remote_path(self):
        if not self.sftp_client:
            messagebox.showinfo("Not connected", "Connect to SSH first.")
            return
        requested_path = self.remote_path_entry.get().strip()
        if not requested_path:
            return

        def _go():
            try:
                normalized_path = requested_path
                if not requested_path.startswith("/"):
                    base_path = self.current_remote_path or "/"
                    normalized_path = os.path.normpath(os.path.join(base_path, requested_path)).replace("\\", "/")
                self.sftp_client.listdir(normalized_path)
                self.current_remote_path = normalized_path
                self.sync_remote_shell_cwd(normalized_path)
                self.after(0, self.refresh_remote_list)
            except Exception as exc:
                self.log_message(f"Remote path change failed: {exc}")
                self.after(0, lambda: messagebox.showerror("Invalid remote path", f"Could not open remote path:\n{requested_path}\n\n{exc}"))
                self.after(0, self.refresh_remote_list)

        threading.Thread(target=_go, daemon=True).start()

    def open_selected_local_file(self, side):
        local_path = self.get_selected_local_path(side)
        if not local_path:
            return
        if os.path.isdir(local_path):
            self.set_local_path(side, local_path)
            self.refresh_local_list(side)
            return
        self.open_file_in_system(local_path)

    def copy_between_local_panes(self, source_side, target_side):
        source_path = self.get_selected_local_path(source_side)
        if not source_path:
            return
        destination_dir = self.get_local_path(target_side)
        destination_path = os.path.join(destination_dir, os.path.basename(source_path))
        if not self.confirm_local_overwrite(destination_path):
            return
        try:
            if os.path.isdir(source_path):
                shutil.copytree(source_path, destination_path, dirs_exist_ok=True)
            else:
                shutil.copy2(source_path, destination_path)
            self.log_message(f"Copied {os.path.basename(source_path)} to {destination_dir}.")
            self.refresh_local_list(target_side)
        except Exception as exc:
            self.log_message(f"Copy failed: {exc}")
            messagebox.showerror("Copy failed", f"Could not copy item: {exc}")

    def move_between_local_panes(self, source_side, target_side):
        source_path = self.get_selected_local_path(source_side)
        if not source_path:
            return
        destination_dir = self.get_local_path(target_side)
        destination_path = os.path.join(destination_dir, os.path.basename(source_path))
        if not self.confirm_local_overwrite(destination_path):
            return
        try:
            if os.path.isdir(destination_path):
                shutil.rmtree(destination_path)
            elif os.path.exists(destination_path):
                os.remove(destination_path)
            shutil.move(source_path, destination_path)
            self.log_message(f"Moved {os.path.basename(source_path)} to {destination_dir}.")
            self.refresh_local_list(source_side)
            self.refresh_local_list(target_side)
        except Exception as exc:
            self.log_message(f"Move failed: {exc}")
            messagebox.showerror("Move failed", f"Could not move item: {exc}")

    def upload_selected_local_file(self, side):
        if not self.sftp_client:
            messagebox.showinfo("Not connected", "Connect to SSH first.")
            return
        local_path = self.get_selected_local_path(side)
        if not local_path:
            return
        if os.path.isdir(local_path):
            messagebox.showinfo("Folder selected", "Select a file to upload.")
            return
        self.upload_file(local_path)

    def delete_selected_local_file(self, side):
        local_path = self.get_selected_local_path(side)
        if not local_path:
            return
        item_name = os.path.basename(local_path)
        if not messagebox.askyesno("Delete Local", f"Delete {item_name}?"):
            return
        try:
            if os.path.isdir(local_path):
                shutil.rmtree(local_path)
            else:
                os.remove(local_path)
            self.log_message(f"Deleted local item: {item_name}.")
            self.refresh_local_list(side)
        except Exception as exc:
            self.log_message(f"Delete failed: {exc}")
            messagebox.showerror("Delete failed", f"Could not delete local item: {exc}")

    def rename_selected_local_item(self, side):
        local_path = self.get_selected_local_path(side)
        if not local_path:
            return
        new_name = self.prompt_new_name("Rename Local Item", os.path.basename(local_path))
        if not new_name:
            return
        new_path = os.path.join(os.path.dirname(local_path), new_name)
        if os.path.exists(new_path) and new_path != local_path:
            messagebox.showerror("Rename failed", f"{new_path}\n\nalready exists.")
            return
        try:
            os.rename(local_path, new_path)
            self.log_message(f"Renamed local item to {new_name}.")
            self.refresh_local_list(side)
        except Exception as exc:
            self.log_message(f"Rename failed: {exc}")
            messagebox.showerror("Rename failed", f"Could not rename local item: {exc}")

    def create_local_folder(self, side):
        folder_name = self.prompt_new_name("New Local Folder", "New Folder")
        if not folder_name:
            return
        folder_path = os.path.join(self.get_local_path(side), folder_name)
        if os.path.exists(folder_path):
            messagebox.showerror("Create folder failed", f"{folder_path}\n\nalready exists.")
            return
        try:
            os.makedirs(folder_path)
            self.log_message(f"Created local folder {folder_name}.")
            self.refresh_local_list(side)
        except Exception as exc:
            self.log_message(f"Create folder failed: {exc}")
            messagebox.showerror("Create folder failed", f"Could not create folder: {exc}")

    def download_selected_remote_file(self, target_side):
        remote_selection = self.get_remote_selected_name()
        if not remote_selection:
            return
        filename, is_directory = remote_selection
        if is_directory:
            return
        self.download_file(filename, self.get_local_path(target_side))

    def download_and_open_selected_remote_file(self):
        remote_selection = self.get_remote_selected_name()
        if not remote_selection:
            return
        filename, is_directory = remote_selection
        if is_directory:
            return
        self.download_file(filename, self.get_local_path("left"), open_after_download=True, refresh_side="left")

    def open_selected_remote_file(self):
        remote_selection = self.get_remote_selected_name()
        if not remote_selection:
            return
        filename, is_directory = remote_selection
        if is_directory:
            return
        self.open_remote_file(filename)

    def rename_selected_remote_item(self):
        remote_selection = self.get_remote_selected_name()
        if not remote_selection:
            return
        item_name, _ = remote_selection
        new_name = self.prompt_new_name("Rename Remote Item", item_name)
        if not new_name or new_name == item_name:
            return
        old_path = f"{self.current_remote_path}/{item_name}".replace("//", "/")
        new_path = f"{self.current_remote_path}/{new_name}".replace("//", "/")
        if self.remote_path_exists(new_path):
            messagebox.showerror("Rename failed", f"{new_path}\n\nalready exists on the server.")
            return
        def _rename():
            try:
                self.sftp_client.rename(old_path, new_path)
                self.log_message(f"Renamed remote item to {new_name}.")
                self.refresh_remote_list()
            except Exception as exc:
                self.log_message(f"Remote rename failed: {exc}")
                self.after(0, lambda: messagebox.showerror("Rename failed", f"Could not rename remote item: {exc}"))
        threading.Thread(target=_rename, daemon=True).start()

    def create_remote_folder(self):
        if not self.sftp_client:
            return
        folder_name = self.prompt_new_name("New Remote Folder", "New Folder")
        if not folder_name:
            return
        remote_path = f"{self.current_remote_path}/{folder_name}".replace("//", "/")
        if self.remote_path_exists(remote_path):
            messagebox.showerror("Create folder failed", f"{remote_path}\n\nalready exists on the server.")
            return
        def _mkdir():
            try:
                self.sftp_client.mkdir(remote_path)
                self.log_message(f"Created remote folder {folder_name}.")
                self.refresh_remote_list()
            except Exception as exc:
                self.log_message(f"Remote mkdir failed: {exc}")
                self.after(0, lambda: messagebox.showerror("Create folder failed", f"Could not create remote folder: {exc}"))
        threading.Thread(target=_mkdir, daemon=True).start()

    def remove_remote_tree(self, remote_path):
        for entry in self.sftp_client.listdir_attr(remote_path):
            child_path = f"{remote_path}/{entry.filename}".replace("//", "/")
            if stat.S_ISDIR(entry.st_mode):
                self.remove_remote_tree(child_path)
            else:
                self.sftp_client.remove(child_path)
        self.sftp_client.rmdir(remote_path)

    def upload_file(self, local_path):
        filename = os.path.basename(local_path)
        remote_path = f"{self.current_remote_path}/{filename}".replace("//", "/")
        if not self.confirm_remote_overwrite(remote_path):
            return

        def _upload():
            try:
                file_size = max(os.path.getsize(local_path), 1)
                def progress(sent, total):
                    self.set_transfer_progress(f"Uploading {filename}...", min(sent / max(total, 1), 1))
                self.log_message(f"Uploading {filename}...")
                self.set_transfer_progress(f"Uploading {filename}...", 0)
                self.sftp_client.put(local_path, remote_path, callback=progress)
                self.log_message("Upload complete.")
                self.refresh_remote_list()
                self.reset_transfer_progress()
            except Exception as exc:
                self.log_message(f"Upload failed: {exc}")
                self.reset_transfer_progress()

        threading.Thread(target=_upload, daemon=True).start()

    def download_file(self, filename, destination_dir, open_after_download=False, refresh_side=None):
        remote_path = f"{self.current_remote_path}/{filename}".replace("//", "/")
        local_path = os.path.join(destination_dir, filename)
        if not self.confirm_local_overwrite(local_path):
            return

        def _download():
            try:
                remote_size = max(self.sftp_client.stat(remote_path).st_size, 1)
                def progress(transferred, total):
                    self.set_transfer_progress(f"Downloading {filename}...", min(transferred / max(total, 1), 1))
                self.log_message(f"Downloading {filename} to {destination_dir}...")
                self.set_transfer_progress(f"Downloading {filename}...", 0)
                self.sftp_client.get(remote_path, local_path, callback=progress)
                self.log_message("Download complete.")
                if refresh_side:
                    self.after(0, lambda: self.refresh_local_list(refresh_side))
                else:
                    if destination_dir == self.local_left_path:
                        self.after(0, lambda: self.refresh_local_list("left"))
                    if destination_dir == self.local_right_path:
                        self.after(0, lambda: self.refresh_local_list("right"))
                if open_after_download:
                    self.after(0, lambda: self.open_file_in_system(local_path))
                self.reset_transfer_progress()
            except Exception as exc:
                self.log_message(f"Download failed: {exc}")
                self.reset_transfer_progress()

        threading.Thread(target=_download, daemon=True).start()

    def open_remote_file(self, filename):
        if self.is_previewable_remote_file(filename):
            self.preview_remote_file(filename)
            return

        remote_path = f"{self.current_remote_path}/{filename}".replace("//", "/")

        def _open():
            temp_path = None
            try:
                suffix = os.path.splitext(filename)[1].lower()
                temp_path = self.create_app_temp_file(suffix)
                self.log_message(f"Opening remote file {filename}...")
                self.sftp_client.get(remote_path, temp_path)
                self.after(0, lambda: self.open_file_in_system(temp_path))
            except Exception as exc:
                if temp_path and os.path.exists(temp_path):
                    os.remove(temp_path)
                self.log_message(f"Open remote file failed: {exc}")
                self.after(0, lambda: messagebox.showerror("Open failed", f"Could not open remote file: {exc}"))

        threading.Thread(target=_open, daemon=True).start()

    def delete_selected_remote_file(self):
        remote_selection = self.get_remote_selected_name()
        if not remote_selection:
            return
        item_name, is_directory = remote_selection
        warning_message = (
            f"Delete remote item {item_name}?\n\n"
            "This will permanently remove it from the remote server.\n"
            "This action is not recoverable."
        )
        if is_directory:
            warning_message += "\n\nIf the folder contains files, they will also be deleted."
        if not messagebox.askyesno("Delete Remote", warning_message):
            return
        remote_path = f"{self.current_remote_path}/{item_name}".replace("//", "/")

        def _delete():
            try:
                if is_directory:
                    self.remove_remote_tree(remote_path)
                else:
                    self.sftp_client.remove(remote_path)
                self.log_message(f"Deleted remote item: {item_name}.")
                self.refresh_remote_list()
            except Exception as exc:
                self.log_message(f"Remote delete failed: {exc}")
                self.after(0, lambda: messagebox.showerror("Delete failed", f"Could not delete remote item: {exc}"))

        threading.Thread(target=_delete, daemon=True).start()

    def preview_remote_file(self, filename):
        remote_path = f"{self.current_remote_path}/{filename}".replace("//", "/")

        def _preview():
            temp_path = None
            try:
                suffix = os.path.splitext(filename)[1].lower() or ".png"
                temp_path = self.create_app_temp_file(suffix)
                self.log_message(f"Fetching preview for {filename}...")
                self.sftp_client.get(remote_path, temp_path)
                self.after(0, lambda: self.show_image_preview(temp_path, filename))
            except Exception as exc:
                if temp_path and os.path.exists(temp_path):
                    os.remove(temp_path)
                self.log_message(f"Preview failed: {exc}")
                self.after(0, lambda: messagebox.showerror("Preview failed", f"Could not preview remote file: {exc}"))

        threading.Thread(target=_preview, daemon=True).start()


if __name__ == "__main__":
    app = SSHClientApp()
    app.mainloop()
