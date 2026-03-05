use maud::{html, Markup};

pub fn render(active_page: &str) -> Markup {
    html! {
        header {
            div class="header" {
                span class="header-logo" { "Homelab" }
            }
            div class="subheader" {
                a href="/" class="subheader-brand" {
                    "Jobfather"
                }
                div class="subheader-nav" {
                    a href="/job-templates" class=(if active_page == "job-templates" { "subheader-nav-item active" } else { "subheader-nav-item" }) { "Job Templates" }
                }
            }
        }
    }
}

pub fn stylesheet_link() -> Markup {
    html! {
        link rel="stylesheet" href="/res/styles.css";
    }
}

pub fn scripts() -> Markup {
    html! {
        script src="/res/htmx.min.js" {}
        script src="/res/idiomorph.min.js" {}
        script src="/res/idiomorph-ext.min.js" {}
    }
}
