use async_trait::async_trait;
use axum::{Extension, Router};
use loco_rs::{
    app::{AppContext, Initializer},
    controller::views::{engines, ViewEngine},
    Result,
};

pub struct ViewEngineInitializer;

#[async_trait]
impl Initializer for ViewEngineInitializer {
    fn name(&self) -> String {
        "view-engine".to_owned()
    }

    async fn after_routes(&self, router: Router, _ctx: &AppContext) -> Result<Router> {
        let engine = engines::TeraView::build()?;
        Ok(router.layer(Extension(ViewEngine::from(engine))))
    }
}
