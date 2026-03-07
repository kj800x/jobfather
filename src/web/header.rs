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
        meta name="viewport" content="width=device-width, initial-scale=1";
        link rel="stylesheet" href="/res/styles.css";
    }
}

pub fn scripts() -> Markup {
    html! {
        script src="/res/htmx.min.js" {}
        script src="/res/idiomorph.min.js" {}
        script src="/res/idiomorph-ext.min.js" {}
        script {
            (maud::PreEscaped(r#"
Idiomorph.defaults.callbacks.beforeAttributeUpdated = function(name, node) { return !(name === 'open' && node.tagName === 'DETAILS'); };
document.addEventListener('click', function(e) {
    var el = e.target.closest('[data-sha]');
    if (!el) return;
    var sha = el.getAttribute('data-sha');
    navigator.clipboard.writeText(sha).then(function() {
        var orig = el.textContent;
        el.textContent = 'Copied!';
        setTimeout(function() { el.textContent = orig; }, 1000);
    });
});
            "#))
        }
    }
}
